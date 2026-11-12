<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import {mathRenderer, mathElement} from "./math-render-service.js";
  import "./math-render.css";
  let {tex, source, preview=source, display=false} = $props();
  let phase=$state("pending"), notice=$state(""), copied=$state(""), revision=$state(0);
  const request=$derived({tex,source,display,revision});
  function render(node,value) {
    let abort, generation=0;
    const update=current => {
      abort?.abort();abort=new AbortController();const turn=++generation;
      node.replaceChildren();phase="pending";notice="";copied="";
      if(typeof globalThis.MathMLElement!=="function"||typeof current.tex!=="string") {
        phase="source-only";return;
      }
      mathRenderer().render(current.tex,current.display,{signal:abort.signal}).then(tree => {
        if(turn!==generation||abort.signal.aborted)return;
        node.replaceChildren(mathElement(tree,node.ownerDocument));phase="ready";
      }).catch(() => {
        if(turn!==generation||abort.signal.aborted)return;
        phase="fallback";notice="Formula could not be rendered. Its original source is shown.";
      });
    };
    update(value);
    return {update,destroy(){generation++;abort?.abort();}};
  }
  async function copySource() {
    try { await navigator.clipboard.writeText(source);copied="Formula source copied"; }
    catch { copied="Select the formula source and copy it with your browser."; }
  }
</script>
<span class="math-formula" class:math-display={display} class:math-inline={!display} data-math-formula>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex (Keyboard users must be able to scroll a wide native formula.) -->
  <span class="math-scroll" data-math-scroll role="region" tabindex="0" aria-label={display ? "Rendered display formula" : "Rendered inline formula"} use:render={request} hidden={phase!=="ready"}></span>
  {#if phase!=="ready"}<code class="math-fallback" aria-label="Formula source">{preview}</code>{/if}
  <details class="math-source">
    <summary title="Show original formula source" aria-label="Show formula source">Source</summary>
    <code class="math-source-text">{source}</code>
    <button class="math-copy" onclick={copySource} aria-label="Copy formula source">Copy source</button>
    {#if phase==="fallback"}<button class="math-retry" onclick={() => revision++}>Render formula</button>{/if}
    {#if copied}<small role="status">{copied}</small>{/if}
  </details>
  {#if notice}<span class="math-render-notice" role="status">{notice}</span>{/if}
</span>
