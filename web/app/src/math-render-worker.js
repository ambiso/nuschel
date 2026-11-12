// SPDX-License-Identifier: EUPL-1.2
// No auto-render, DOM, URLs, SDK/session metadata or persistent macros.
import {renderFormula} from "./math-render-core.js";
postMessage({kind:"ready"});
onmessage = ({data}) => {
  if (!Number.isSafeInteger(data?.id) || data.id < 1) return;
  try { postMessage({kind:"result", id:data.id, tree:JSON.stringify(renderFormula(data.tex, data.display))}); }
  catch { postMessage({kind:"error", id:data.id, message:"This formula could not be rendered. Its source is available."}); }
};
