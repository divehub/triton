// Keyboard shortcuts of the handset buttons: ArrowUp = Up, ArrowDown = Down, Enter = Confirm.
//
// A shortcut applies only when the key press is not meant for something else on the page. Fields, selects, links
// and disclosure summaries keep every one of these keys (opening Advanced or a "Sensor variations" section with the
// keyboard must never navigate or confirm in the guest); a focused button yields only Enter, which activates it
// (arrow keys have no other meaning there, and a mouse click gives the focus back to the page, see emulator.js).
// The UART console, the history lists and modal dialogs use the keys for reading and choosing.
//
// DOM-free (the event is only inspected): the Node tests call it with plain objects.

const KEYS = Object.freeze({ ArrowUp: 'up', ArrowDown: 'down', Enter: 'confirm' });
const FIELDS = /^(INPUT|TEXTAREA|SELECT|SUMMARY|A)$/;

/** The handset action ('up' | 'down' | 'confirm') a keydown event stands for, or null to leave it alone. */
export function handsetKeyAction(event) {
  if (!event || event.defaultPrevented || event.repeat) return null;
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return null;
  const action = KEYS[event.key];
  if (!action) return null;
  const target = event.target;
  if (target && typeof target.closest === 'function' && target.closest('#uart-panel, .output-history, dialog')) return null;
  const tag = target && target.tagName ? String(target.tagName).toUpperCase() : '';
  if (FIELDS.test(tag) || (target && target.isContentEditable)) return null;
  if (tag === 'BUTTON' && event.key === 'Enter') return null;
  return action;
}
