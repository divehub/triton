// The water view's scene (DESIGN 22): everything the dive game draws that moves with the depth.
//
// One depth story, from the boat to the dark: a camera follows the diver through a fixed 30 m window; particles ("marine snow"),
// vent bubbles, the splash of the entry and the torch are drawn on one canvas, anchored in the world so they stream past as the
// diver climbs and sinks; the boat, the water, the depth lines and the rays are page elements that this scene only moves.
//
// This file has no DOM: `step` advances the scene and fills `view` (positions in pixels, angles in degrees, flags) for game.js to
// put on the page, and `draw` paints onto a 2D context it is given. The decisions (camera target, smoothing, torch hysteresis,
// the entry's phases, the bubble count) are the pure functions of game-logic.js. Nothing here feeds back into the dive simulation
// or the emulator: the scene is a picture of the depth the simulation already has.

import { MAX_DEPTH_METERS } from './game-gas.js';
import {
  BubbleEmitter, Camera, DEPTH_SMOOTH_S, ENTRY_SPLASH_AT, EntryState, TORCH_CONE, VIEW_WINDOW_M, coneLight, entryPose, lightFade,
  smoothDamp, torchNext,
} from './game-logic.js';

const TAU = Math.PI * 2;
const clamp = (value, low, high) => Math.min(high, Math.max(low, value));
const wrap = (value, length) => ((value % length) + length) % length;

/** Where the diver floats, as a fraction of the panel width; the sprite is 140 px wide. */
export const DIVER_X = 0.36;
/** The sprite's center sits this far above the depth the sensors read (meters). */
const BODY_OFFSET_M = 0.25;
/** The torch lens and the loop's vent, from the sprite's center in the swimming pose (pixels, +x is forward, +y is down). */
export const TORCH_LENS = Object.freeze({ x: 57, y: 5 });
export const VENT_POINT = Object.freeze({ x: -6, y: -24 });
const TORCH_TILT_DEG = 4; // the beam points a little below the swimming line
const ATTITUDE_DEG = 8; // the diver tilts down when descending and up when ascending
/** The longest time one animation frame advances the scene (wall seconds). */
export const MAX_FRAME_STEP_S = 0.1;

export const PARTICLE_COUNT = 150;
// Three depths of marine snow: far (moves slower than the world), the world's own, and near (moves faster). Sizes in pixels.
export const PARTICLE_LAYERS = Object.freeze([
  { factor: 0.55, share: 0.4, size: [0.8, 1.4], alpha: 0.26 },
  { factor: 1, share: 0.4, size: [1.2, 2.2], alpha: 0.38 },
  { factor: 1.5, share: 0.2, size: [2, 3.2], alpha: 0.17 },
]);

export const BUBBLE_CAP = 320;
export const BUBBLES_PER_FRAME = 14;
export const BUBBLE_QUEUE_MAX = 60; // bubbles waiting for a frame: a larger burst (Uncapped) is thinned, not replayed later
const BUBBLE_RISE = [0.85, 1.25]; // m/s: exaggerated so a bubble crosses the panel in a few seconds
const BUBBLE_RADIUS = [1.8, 4.2]; // px at release, before the flow makes them bigger
const BUBBLE_MIN_DEPTH_M = 0.15;

const lerp = (low, high, fraction) => low + (high - low) * fraction;

export class WaterScene {
  /** @param {{random?: () => number, reducedMotion?: boolean}} [options] */
  constructor({ random = Math.random, reducedMotion = false } = {}) {
    this.random = random;
    this.reducedMotion = reducedMotion;
    this.camera = new Camera();
    this.entry = new EntryState();
    this.emitter = new BubbleEmitter();
    this.particles = this.makeParticles(); // one list per layer
    this.bubbles = [];
    this.drops = [];
    this.ripples = [];
    this.layout = { width: 0, height: 0, ppm: 0 };
    // What game.js puts on the page after each step.
    this.view = {
      cameraTop: 0, depth: 0, phase: 'boat', progress: 0,
      diver: { x: 0, y: 0, spin: 0, scale: 1, attitude: 0 },
      boat: { y: 0, tilt: 0 },
      torch: { on: false, level: 0, x: 0, y: 0, angle: 0 },
      rayFade: 1, // how much of the surface light reaches the window: the rays and the far shapes fade with it
      shapeFade: 1,
    };
    this.reset();
  }

  /** The start of a session and Reset dive: the diver is on the boat at the surface, the camera at the top, no torch, no bubbles. */
  reset() {
    this.camera.snap(0);
    this.entry.reset();
    this.emitter.reset();
    this.queued = 0; // bubbles the vent made that no frame has released yet
    this.bubbles.length = 0;
    this.drops.length = 0;
    this.ripples.length = 0;
    this.depth = 0; // the drawn depth: follows the simulation's smoothly
    this.depthVelocity = 0;
    this.torchOn = false;
    this.torchLevel = 0;
    this.attitude = 0;
    this.ventLevel = 0; // the recent vent rate in surface liters per second (it sizes the bubbles)
    this.time = 0; // ambient time: it drives the bobbing only and stops with a pause
    this.dirty = true;
  }

  setReducedMotion(flag) {
    if (this.reducedMotion === !!flag) return;
    this.reducedMotion = !!flag;
    this.dirty = true;
  }

  /** Marine snow: positions as a fraction of the width and meters inside the window; they wrap around, so the field never ends. */
  makeParticles() {
    return PARTICLE_LAYERS.map((layer) => {
      const list = [];
      const count = Math.round(PARTICLE_COUNT * layer.share);
      for (let n = 0; n < count; n++) {
        list.push({
          x: this.random(),
          y: this.random() * VIEW_WINDOW_M,
          size: lerp(layer.size[0], layer.size[1], this.random()),
          sway: 2 + this.random() * 6, // px
          phase: this.random() * TAU,
          sink: 0.04 + this.random() * 0.14, // m/s, the slow fall of marine snow
        });
      }
      return list;
    });
  }

  /**
   * Advances the scene by `dt` wall seconds (0 while paused: the camera holds, the drift and the bubbles stop) for a diver at
   * `depth` meters moving `direction` (-1 up, 0 hold, 1 down), with `vented` surface liters of loop gas released since the last
   * step. `layout` is the panel in pixels: {width, height, ppm} (pixels per meter of the fixed window); `timeScale` is how many
   * seconds of the dive pass per wall second (1, 2, 4; Uncapped counts as 4), which the bubbles' rise follows.
   */
  step({ dt = 0, depth = 0, direction = 0, vented = 0, layout, timeScale = 1 }) {
    const reduced = this.reducedMotion;
    const step = dt > 0 ? dt : 0; // game.js passes at most MAX_FRAME_STEP_S: a hidden tab does not make the scene jump
    if (layout.width !== this.layout.width || layout.height !== this.layout.height || layout.ppm !== this.layout.ppm) this.dirty = true;
    this.layout = layout;
    const { ppm, width } = layout;

    // The drawn depth follows the simulation's (the states come five times a second); the camera follows the drawn depth.
    const truth = clamp(Number.isFinite(depth) ? depth : 0, 0, MAX_DEPTH_METERS);
    if (reduced) {
      this.depth = truth;
      this.depthVelocity = 0;
    } else {
      const next = smoothDamp(this.depth, this.depthVelocity, truth, DEPTH_SMOOTH_S, step);
      this.depth = clamp(next.position, 0, MAX_DEPTH_METERS);
      this.depthVelocity = next.velocity;
    }
    const cameraTop = this.camera.update(this.depth, step, { ease: !reduced });

    const entry = this.entry.update({ dt: step, depth: truth, direction, reducedMotion: reduced });
    this.torchOn = torchNext(truth, this.torchOn);
    const torchGoal = this.torchOn ? 1 : 0;
    this.torchLevel = reduced ? torchGoal : this.torchLevel + clamp(torchGoal - this.torchLevel, -step * 4, step * 4);
    const attitudeGoal = direction * ATTITUDE_DEG;
    if (reduced) this.attitude = attitudeGoal;
    else if (step > 0) this.attitude += (attitudeGoal - this.attitude) * (1 - Math.exp(-step / 0.18));
    if (step > 0) this.ventLevel = this.ventLevel * Math.exp(-step / 0.5) + Math.max(0, vented) / 0.5;
    if (!reduced) this.time += step;
    if (step > 0) this.dirty = true;

    // The picture of the diver: in the water, or sitting on the boat and rolling off it.
    const inWater = entry.phase === 'water';
    const pose = inWater ? { dx: 0, dy: 0, spin: 0, scale: 1 } : entryPose(entry.progress);
    const bob = reduced ? 0 : Math.sin(this.time * 1.3) * 1.7;
    const boatHold = entry.phase === 'boat' ? 1 : entry.phase === 'entering' ? Math.max(0, 1 - entry.progress / ENTRY_SPLASH_AT) : 0;
    const restX = width * DIVER_X;
    const restY = (this.depth - cameraTop - BODY_OFFSET_M) * ppm;
    const view = this.view;
    view.cameraTop = cameraTop;
    view.depth = this.depth;
    view.phase = entry.phase;
    view.progress = entry.progress;
    view.diver.x = restX + pose.dx;
    view.diver.y = restY + pose.dy + bob * boatHold;
    view.diver.spin = pose.spin;
    view.diver.scale = pose.scale;
    view.diver.attitude = inWater || entry.phase === 'entering' ? this.attitude : 0;
    view.boat.y = bob;
    view.boat.tilt = reduced ? 0 : Math.sin(this.time * 1.1 + 1.2) * 0.45;
    view.rayFade = lightFade(cameraTop, 48);
    view.shapeFade = lightFade(cameraTop, 72);
    const angle = ((view.diver.attitude + TORCH_TILT_DEG) * Math.PI) / 180;
    const lean = (view.diver.attitude * Math.PI) / 180;
    const rotate = (point, radians) => ({ x: point.x * Math.cos(radians) - point.y * Math.sin(radians), y: point.x * Math.sin(radians) + point.y * Math.cos(radians) });
    const lens = rotate(TORCH_LENS, lean);
    const torch = view.torch;
    torch.on = this.torchOn;
    torch.level = this.torchLevel;
    torch.x = view.diver.x + lens.x;
    torch.y = view.diver.y + lens.y;
    torch.angle = angle;

    this.driftParticles(step);
    // The bubbles rise on the dive's clock: at 4x they rise four times as fast on the screen, as the diver swims.
    this.moveBubbles(step * clamp(timeScale, 1, 4));
    // The vent: only the gas the loop released becomes bubbles, only while time runs and the diver is in the water. A burst (the
    // states come five times a second) is released over a few frames, never more than BUBBLES_PER_FRAME at once.
    if (step > 0) {
      if (inWater && this.depth > BUBBLE_MIN_DEPTH_M) {
        if (vented > 0) this.queued = Math.min(BUBBLE_QUEUE_MAX, this.queued + this.emitter.emit(vented));
        const count = Math.min(this.queued, BUBBLES_PER_FRAME);
        if (count > 0) {
          this.queued -= count;
          const origin = rotate(VENT_POINT, lean);
          this.releaseBubbles(count, this.ventLevel, view.diver.x + origin.x, view.diver.y + origin.y, ppm, cameraTop, step);
        }
      } else {
        this.queued = 0; // on the boat or at the surface the loop's gas goes nowhere to be seen
      }
    }
    if (entry.splash) this.splash(view.diver.x);
    this.moveSplash(step);
    return view;
  }

  driftParticles(step) {
    if (this.reducedMotion || step === 0) return;
    for (const layer of this.particles) {
      for (const particle of layer) {
        particle.y += particle.sink * step;
        particle.phase += step * (0.4 + particle.sway * 0.05);
      }
    }
  }

  // ---- bubbles ----

  /** Releases `count` bubbles at (x, y) px, the vent; `flow` (surface liters per second) makes them a little bigger. */
  releaseBubbles(count, flow, x, y, ppm, cameraTop, step) {
    if (!(count > 0) || !(ppm > 0)) return;
    const big = clamp(flow / 1.5, 0, 1);
    const releaseDepth = cameraTop + y / ppm;
    for (let n = 0; n < Math.min(count, BUBBLES_PER_FRAME); n++) {
      if (this.bubbles.length >= BUBBLE_CAP) break;
      const speed = lerp(BUBBLE_RISE[0], BUBBLE_RISE[1], this.random());
      // The gas was released at some point of the last moment, not all at the end of it: spread the bubbles along their rise.
      const age = this.random() * Math.max(step, 0.2);
      this.bubbles.push({
        x: x + (this.random() - 0.5) * 7,
        depth: releaseDepth - speed * age,
        radius: lerp(BUBBLE_RADIUS[0], BUBBLE_RADIUS[1], this.random()) * (1 + 0.7 * big),
        speed,
        pressure: 1 + releaseDepth / 10, // bar, roughly: the bubble grows as the water above it gets less
        phase: this.random() * TAU,
        amp: 1 + this.random() * 3,
      });
    }
  }

  moveBubbles(step) {
    if (step === 0) return;
    let kept = 0;
    for (const bubble of this.bubbles) {
      bubble.depth -= bubble.speed * step;
      bubble.phase += step * 3;
      if (bubble.depth > 0) this.bubbles[kept++] = bubble; // at the surface a bubble is gone
    }
    this.bubbles.length = kept;
  }

  // ---- the splash ----

  /** The diver reaches the water: spray, two ripples and some foam at the surface. */
  splash(x) {
    for (let n = 0; n < 26; n++) {
      const direction = -Math.PI / 2 + (this.random() - 0.5) * 1.9;
      const speed = 90 + this.random() * 170;
      this.drops.push({
        x: x + (this.random() - 0.5) * 30, height: 0, vx: Math.cos(direction) * speed * 0.7, vh: -Math.sin(direction) * speed,
        age: 0, life: 0.55 + this.random() * 0.4, radius: 1 + this.random() * 1.8,
      });
    }
    for (let n = 0; n < 2; n++) this.ripples.push({ x, age: -n * 0.14, life: 0.95, radius: 46 + 26 * n });
    for (let n = 0; n < 9; n++) {
      this.ripples.push({ x: x + (this.random() - 0.5) * 70, age: 0, life: 1.1 + this.random() * 0.4, radius: 4 + this.random() * 7, foam: true });
    }
  }

  moveSplash(step) {
    if (step === 0) return;
    let kept = 0;
    for (const drop of this.drops) {
      drop.age += step;
      drop.x += drop.vx * step;
      drop.height += drop.vh * step;
      drop.vh -= 560 * step;
      if (drop.age < drop.life && drop.height > -2) this.drops[kept++] = drop;
    }
    this.drops.length = kept;
    kept = 0;
    for (const ripple of this.ripples) {
      ripple.age += step;
      if (ripple.age < ripple.life) this.ripples[kept++] = ripple;
    }
    this.ripples.length = kept;
  }

  // ---- drawing ----

  /** Paints the canvas layer: marine snow, the splash, the torch's light with the particles it lights, and the bubbles. */
  draw(ctx) {
    const { width, height, ppm } = this.layout;
    if (!(width > 0) || !(height > 0) || !(ppm > 0)) return;
    ctx.clearRect(0, 0, width, height);
    const cameraTop = this.view.cameraTop;
    const surfaceY = -cameraTop * ppm;
    const floorY = (MAX_DEPTH_METERS - cameraTop) * ppm;
    const torch = this.view.torch;
    const lit = torch.level > 0.01;
    const lighted = [];
    for (let layerIndex = 0; layerIndex < PARTICLE_LAYERS.length; layerIndex++) {
      const layer = PARTICLE_LAYERS[layerIndex];
      const factor = this.reducedMotion ? 1 : layer.factor;
      ctx.fillStyle = `rgba(214, 240, 240, ${layer.alpha})`;
      ctx.beginPath();
      for (const particle of this.particles[layerIndex]) {
        const y = wrap(particle.y - cameraTop * factor, VIEW_WINDOW_M) * ppm;
        if (y < surfaceY + 4 || y > floorY - 4) continue;
        const x = particle.x * width + (this.reducedMotion ? 0 : Math.sin(particle.phase) * particle.sway);
        const light = lit ? coneLight(x - torch.x, y - torch.y, torch.angle) * torch.level : 0;
        if (light > 0.02) {
          lighted.push(x, y, particle.size, light);
        } else {
          ctx.moveTo(x + particle.size, y);
          ctx.arc(x, y, particle.size, 0, TAU);
        }
      }
      ctx.fill();
    }
    if (lit) this.drawTorch(ctx, torch);
    for (let index = 0; index < lighted.length; index += 4) {
      const light = lighted[index + 3];
      ctx.fillStyle = `rgba(255, 247, 220, ${Math.min(1, 0.3 + light * 1.3).toFixed(3)})`;
      ctx.beginPath();
      ctx.arc(lighted[index], lighted[index + 1], lighted[index + 2] * (1 + light * 1.1), 0, TAU);
      ctx.fill();
    }
    this.drawBubbles(ctx, cameraTop, ppm, height);
    this.drawSplash(ctx, surfaceY);
    this.dirty = false;
  }

  /** A soft cone in front of the diver: three wedges of increasing width, each fading with distance, added to what is behind. */
  drawTorch(ctx, torch) {
    const length = TORCH_CONE.length;
    ctx.save();
    ctx.globalCompositeOperation = 'lighter';
    for (const [spread, alpha] of [[1, 0.03], [0.75, 0.045], [0.5, 0.06], [0.28, 0.07]]) {
      const half = TORCH_CONE.halfAngle * spread;
      const gradient = ctx.createRadialGradient(torch.x, torch.y, 3, torch.x, torch.y, length);
      gradient.addColorStop(0, `rgba(255, 244, 208, ${(alpha * 3 * torch.level).toFixed(3)})`);
      gradient.addColorStop(0.35, `rgba(255, 240, 200, ${(alpha * 1.7 * torch.level).toFixed(3)})`);
      gradient.addColorStop(0.7, `rgba(255, 238, 195, ${(alpha * 0.7 * torch.level).toFixed(3)})`);
      gradient.addColorStop(1, 'rgba(255, 236, 190, 0)');
      ctx.fillStyle = gradient;
      ctx.beginPath();
      ctx.moveTo(torch.x, torch.y);
      ctx.lineTo(torch.x + Math.cos(torch.angle - half) * length, torch.y + Math.sin(torch.angle - half) * length);
      ctx.arc(torch.x, torch.y, length, torch.angle - half, torch.angle + half);
      ctx.closePath();
      ctx.fill();
    }
    const glow = ctx.createRadialGradient(torch.x, torch.y, 0, torch.x, torch.y, 17);
    glow.addColorStop(0, `rgba(255, 250, 225, ${(0.7 * torch.level).toFixed(3)})`);
    glow.addColorStop(1, 'rgba(255, 244, 205, 0)');
    ctx.fillStyle = glow;
    ctx.beginPath();
    ctx.arc(torch.x, torch.y, 17, 0, TAU);
    ctx.fill();
    ctx.restore();
  }

  drawBubbles(ctx, cameraTop, ppm, height) {
    if (this.bubbles.length === 0) return;
    ctx.lineWidth = 1;
    ctx.strokeStyle = 'rgba(226, 251, 247, 0.72)';
    ctx.fillStyle = 'rgba(190, 236, 236, 0.15)';
    ctx.beginPath();
    const shines = [];
    for (const bubble of this.bubbles) {
      const y = (bubble.depth - cameraTop) * ppm;
      // Boyle: the bubble grows as the pressure around it falls.
      const radius = bubble.radius * Math.cbrt(bubble.pressure / (1 + bubble.depth / 10));
      if (y < -radius || y > height + radius) continue;
      const x = bubble.x + (this.reducedMotion ? 0 : Math.sin(bubble.phase) * bubble.amp);
      ctx.moveTo(x + radius, y);
      ctx.arc(x, y, radius, 0, TAU);
      shines.push(x - radius * 0.35, y - radius * 0.35, Math.max(0.7, radius * 0.22));
    }
    ctx.fill();
    ctx.stroke();
    ctx.fillStyle = 'rgba(255, 255, 255, 0.85)';
    ctx.beginPath();
    for (let index = 0; index < shines.length; index += 3) {
      ctx.moveTo(shines[index] + shines[index + 2], shines[index + 1]);
      ctx.arc(shines[index], shines[index + 1], shines[index + 2], 0, TAU);
    }
    ctx.fill();
  }

  drawSplash(ctx, surfaceY) {
    if (this.drops.length === 0 && this.ripples.length === 0) return;
    for (const ripple of this.ripples) {
      if (ripple.age < 0) continue;
      const fraction = ripple.age / ripple.life;
      ctx.beginPath();
      if (ripple.foam) {
        ctx.fillStyle = `rgba(236, 252, 248, ${(0.55 * (1 - fraction)).toFixed(3)})`;
        ctx.ellipse(ripple.x, surfaceY + 1, ripple.radius * (0.6 + fraction * 0.5), ripple.radius * 0.22, 0, 0, TAU);
        ctx.fill();
      } else {
        ctx.strokeStyle = `rgba(226, 251, 247, ${(0.6 * (1 - fraction)).toFixed(3)})`;
        ctx.lineWidth = 1.5;
        ctx.ellipse(ripple.x, surfaceY + 1, ripple.radius * Math.sqrt(fraction), ripple.radius * 0.2 * Math.sqrt(fraction), 0, 0, TAU);
        ctx.stroke();
      }
    }
    ctx.beginPath();
    ctx.fillStyle = 'rgba(238, 253, 250, 0.82)';
    for (const drop of this.drops) {
      const y = surfaceY - drop.height;
      ctx.moveTo(drop.x + drop.radius, y);
      ctx.arc(drop.x, y, drop.radius * (1 - 0.5 * drop.age / drop.life), 0, TAU);
    }
    ctx.fill();
  }
}
