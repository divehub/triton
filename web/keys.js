// Keyboard shortcuts of the handset buttons: ArrowUp = Up, ArrowDown = Down, Enter = Confirm.
//
// The Up and Down arrow keys belong to the handset only, wherever the focus is: while a session runs they never scroll
// the page, move a select or a number field, or read the UART console (`isHandsetArrow` tells the views to cancel
// them; a held key is one press). Only an open modal dialog keeps them. Enter is different: it applies only when the
// key press is not meant for something else on the page. Fields, selects, links and disclosure summaries keep it
// (opening Advanced or a "Sensor variations" section with the keyboard must never confirm in the guest), a focused
// button keeps it (it activates the button), and so do the UART console, the history lists and modal dialogs.
//
// DOM-free (the event is only inspected): the Node tests call it with plain objects.

const ARROWS = Object.freeze({ ArrowUp: 'up', ArrowDown: 'down' });
const FIELDS = /^(INPUT|TEXTAREA|SELECT|SUMMARY|A)$/;

const inside = (target, selector) => !!(target && typeof target.closest === 'function' && target.closest(selector));

/** True for an Up or Down arrow key press (held or not) outside a modal dialog: the views cancel its default action. */
export function isHandsetArrow(event) {
  if (!event || !ARROWS[event.key] || event.altKey || event.ctrlKey || event.metaKey) return false;
  return !inside(event.target, 'dialog');
}

/** The handset action ('up' | 'down' | 'confirm') a keydown event stands for, or null to leave it alone. */
export function handsetKeyAction(event) {
  if (!event || event.defaultPrevented || event.repeat) return null;
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return null;
  const target = event.target;
  if (ARROWS[event.key]) return inside(target, 'dialog') ? null : ARROWS[event.key];
  if (event.key !== 'Enter') return null;
  if (inside(target, '#uart-panel, .output-history, dialog')) return null;
  const tag = target && target.tagName ? String(target.tagName).toUpperCase() : '';
  if (FIELDS.test(tag) || (target && target.isContentEditable) || tag === 'BUTTON') return null;
  return 'confirm';
}
