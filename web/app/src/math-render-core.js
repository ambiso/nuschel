// SPDX-License-Identifier: EUPL-1.2
// Published core ESM source, excluding the default dist bundle's extensions.
// This internal tree API is pinned; unexpected shapes fail to source fallback.
import temml from "temml/temml.js";
import {formulaInput, projectMathTree} from "./math-render-protocol.js";
export function renderFormula(tex, display) {
  formulaInput(tex, display);
  return projectMathTree(temml.__renderToMathMLTree(tex, {
    displayMode:display, trust:false, strict:true, throwOnError:true,
    maxExpand:100, maxSize:[8,96], macros:Object.create(null),
    annotate:false, xml:false,
  }));
}
