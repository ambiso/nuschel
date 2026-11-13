// SPDX-License-Identifier: EUPL-1.2
// Horizontal swipes for the mobile navigation drawer: a right swipe on the chat
// opens it, a left swipe on the drawer closes it. Pure decisions over pointer
// samples ({x, y, t}: CSS pixels and milliseconds), and which targets keep their drags.
export const SWIPE = Object.freeze({
  slop: 10, // travel before the axis is decided
  dominance: 1.5, // horizontal travel must exceed vertical travel this many times
  hold: 450, // a press held this long before moving is a long press (selection)
  distance: 64, // travel that commits on release
  fling: 24, // the least travel a fast swipe commits with
  velocity: 0.35, // px/ms along the swipe that commits a fling
  window: 100, // ms of recent samples that give the release velocity
  samples: 8, // samples kept per gesture
});

// direction: 1 swipes right, -1 swipes left.
export function startSwipe(point, direction) {
  return {direction, start:point, last:point, samples:[point], axis:"pending"};
}

export function moveSwipe(swipe, point) {
  if (swipe.axis === "rejected") return swipe;
  const dx = point.x - swipe.start.x, dy = point.y - swipe.start.y;
  let axis = swipe.axis;
  if (axis === "pending") {
    if (Math.hypot(dx, dy) >= SWIPE.slop) {
      axis = dx * swipe.direction > 0 && Math.abs(dx) >= SWIPE.dominance * Math.abs(dy) ? "horizontal" : "rejected";
    } else if (point.t - swipe.start.t > SWIPE.hold) axis = "rejected";
  }
  const samples = [...swipe.samples, point].slice(-SWIPE.samples);
  return {...swipe, last:point, samples, axis};
}

// Velocity in the swipe's direction over the last SWIPE.window milliseconds.
export function releaseVelocity(swipe) {
  const {samples, last, direction} = swipe;
  const first = samples.find(sample => sample !== last && last.t - sample.t <= SWIPE.window) ?? samples.at(-2);
  if (!first || last.t <= first.t) return 0;
  return (last.x - first.x) * direction / (last.t - first.t);
}

// Whether releasing here completes the swipe.
export function endSwipe(swipe) {
  if (swipe.axis !== "horizontal") return false;
  const travel = (swipe.last.x - swipe.start.x) * swipe.direction, velocity = releaseVelocity(swipe);
  if (travel < Math.abs(swipe.last.y - swipe.start.y)) return false;
  if (velocity >= SWIPE.velocity && travel >= SWIPE.fling) return true;
  return travel >= SWIPE.distance && velocity > -SWIPE.velocity;
}

// Where a horizontal drag already means something, it keeps that meaning: text
// fields, code, formulas and images, drag handles, and anything the browser can
// pan sideways.
const EXEMPT = "input,textarea,select,[contenteditable],pre,canvas,video,.code-block,.image-button,.math-scroll,.server-order-handle";
export function swipeExempt(target, root) {
  if (target.closest(EXEMPT)) return true;
  for (let node = target; node && node !== root; node = node.parentElement) {
    const {overflowX, touchAction} = getComputedStyle(node);
    const pansX = touchAction === "auto" || touchAction === "manipulation" || /pan-(x|left|right)/.test(touchAction);
    if ((overflowX === "auto" || overflowX === "scroll") && pansX && node.scrollWidth > node.clientWidth + 1) return true;
  }
  return false;
}
