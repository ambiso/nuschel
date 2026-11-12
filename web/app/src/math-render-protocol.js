// SPDX-License-Identifier: EUPL-1.2
// A deliberately small inert MathML dialect. No HTML, SVG, URLs, CSS, classes,
// annotations, IDs or event attributes cross the worker boundary.
export const MAX_TEX_BYTES = 4096, MAX_TREE_BYTES = 65536, MAX_NODES = 4096, MAX_DEPTH = 48;
const encoder = new TextEncoder();
const tags = new Set("math mrow mi mn mo mtext mfrac msqrt mroot msup msub msubsup munder mover munderover mstyle mspace mpadded mphantom mtable mtr mtd menclose mmultiscripts mprescripts none".split(" "));
const booleans = new Set("displaystyle stretchy symmetric largeop movablelimits accent accentunder fence separator".split(" "));
const lengths = new Set("width height depth lspace rspace voffset linethickness".split(" "));
const variants = new Set("normal bold italic bold-italic double-struck bold-fraktur script bold-script fraktur sans-serif bold-sans-serif sans-serif-italic sans-serif-bold-italic monospace initial tailed looped stretched".split(" "));
const fail = () => { throw new Error("This formula requires unsupported or oversized MathML; its source is preserved."); };
export function boundedText(value, limit) {
  if (typeof value !== "string" || value.length > limit || encoder.encode(value).length > limit) fail();
  return value;
}
export function formulaInput(tex, display) {
  boundedText(tex, MAX_TEX_BYTES);
  if (!tex.trim() || typeof display !== "boolean") fail();
}
const record = value => value && Object.getPrototypeOf(value) === Object.prototype;
function length(value) {
  const match = /^(-?\d{1,3}(?:\.\d{1,6})?)(em|ex|pt|px)?$/.exec(value);
  if (!match || Math.abs(Number(match[1])) > (match[2] === "pt" || match[2] === "px" ? 96 : 8)) fail();
}
function attribute(name, value) {
  boundedText(value, 128);
  if (booleans.has(name)) { if (!["true", "false"].includes(value)) fail(); }
  else if (lengths.has(name)) length(value);
  else if (name === "display") { if (!["block", "inline"].includes(value)) fail(); }
  else if (name === "mathvariant") { if (!variants.has(value)) fail(); }
  else if (name === "form") { if (!["prefix", "infix", "postfix"].includes(value)) fail(); }
  else if (name === "scriptlevel") { if (!/^[0-2]$/.test(value)) fail(); }
  else if (["rowspan", "columnspan"].includes(name)) { if (!/^(?:[1-9]|1[0-6])$/.test(value)) fail(); }
  else if (["columnalign", "rowalign"].includes(name)) { if (!value.split(" ").every(part => ["left", "center", "right", "top", "bottom", "baseline", "axis"].includes(part))) fail(); }
  else if (["columnspacing", "rowspacing"].includes(name)) { for (const part of value.split(" ")) length(part); }
  else if (name === "notation") { if (!value.split(" ").every(part => ["box", "roundedbox", "circle", "longdiv", "actuarial", "radical", "top", "bottom", "left", "right", "horizontalstrike", "verticalstrike", "updiagonalstrike", "downdiagonalstrike", "updiagonalarrow", "phasorangle"].includes(part))) fail(); }
  else fail();
}
function visit(node, budget, depth, project) {
  if (++budget.nodes > MAX_NODES || depth > MAX_DEPTH || !node || typeof node !== "object") fail();
  if (Object.hasOwn(node, "text")) {
    if (Object.keys(node).some(key => key !== "text")) fail();
    const text = boundedText(node.text, 16384); budget.text += encoder.encode(text).length;
    if (budget.text > 16384) fail();
    return {text};
  }
  let tag, attrs, children;
  if (project) {
    // Temml's pinned internal MathNode contains classes/style metadata. It is
    // deliberately discarded, never interpreted or forwarded as attributes.
    // Its document fragments become ordinary MathML grouping rows.
    if (Object.keys(node).some(key => !["type", "attributes", "children", "classes", "style", "label"].includes(key))) fail();
    if (node.classes !== undefined && (!Array.isArray(node.classes) || node.classes.length > 32 || node.classes.some(value => typeof value !== "string" || value.length > 128))) fail();
    if (node.style !== undefined && (!record(node.style) || Object.keys(node.style).length > 32)) fail();
    tag = node.type;
    if (tag === undefined && node.attributes === undefined && node.label === undefined) tag = "mrow";
    attrs = node.attributes ?? {}; children = node.children;
  } else {
    if (!record(node) || Object.keys(node).some(key => !["tag", "attrs", "children"].includes(key))) fail();
    tag = node.tag; attrs = node.attrs; children = node.children;
  }
  if (!tags.has(tag) || !record(attrs) || Object.keys(attrs).length > 16 || !Array.isArray(children) || children.length > MAX_NODES) fail();
  const safeAttrs = {};
  for (const [name, value] of Object.entries(attrs)) { attribute(name, value); safeAttrs[name] = value; }
  return {tag, attrs:safeAttrs, children:children.map(child => visit(child, budget, depth + 1, project))};
}
export function validateMathTree(tree) {
  const result = visit(tree, {nodes:0,text:0}, 0, false);
  if (result.tag !== "math") fail();
  boundedText(JSON.stringify(result), MAX_TREE_BYTES);
  return result;
}
export function projectMathTree(tree) {
  return validateMathTree(visit(tree, {nodes:0,text:0}, 0, true));
}
export function parseMathReply(json) {
  boundedText(json, MAX_TREE_BYTES);
  return validateMathTree(JSON.parse(json));
}
