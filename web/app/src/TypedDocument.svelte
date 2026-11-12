<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { visibleBlocks, codeColor, textRuns, rgbaPixels } from "./presentation.js";
  import MathFormula from "./MathFormula.svelte";
  let { document: content } = $props();
  let expanded = $state(false);
  let copied = $state("");
  let viewer = $state(null);
  const shown = $derived(visibleBlocks(content, expanded));
  function pixels(canvas, block) {
    const paint = (value) => {
      const rgba = rgbaPixels(value);
      if (!rgba) { canvas.width = 1; canvas.height = 1; canvas.dataset.invalid = "true"; return; }
      canvas.width = value.width; canvas.height = value.height;
      canvas.getContext("2d").putImageData(new ImageData(rgba, value.width, value.height), 0, 0);
    };
    paint(block);
    return { update: paint };
  }
  function dialog(node) { node.showModal(); return { destroy() { node.close(); } }; }
  async function copyCode(block) {
    try { await navigator.clipboard.writeText(block.runs.map((run) => run.text).join("")); copied = "Code copied"; }
    catch { copied = "Select the code and copy it with your browser."; }
  }
</script>
<div class="typed-document">
    {#each shown.blocks as block}
      {#if block.kind === "code"}
        <div class="code-block"><div class="code-toolbar"><span>{block.language || "Code"}</span><button onclick={() => copyCode(block)} aria-label="Copy code">Copy</button></div><pre aria-label={block.language ? `${block.language} code` : "Code"}><code>{#each block.runs as run}<span style:color={codeColor(run.foreground)} style:font-weight={run.bold ? "bold" : "normal"} style:font-style={run.italic ? "italic" : "normal"}>{run.text}</span>{/each}</code></pre></div>
      {:else if block.kind === "image"}
        <button class="image-button" aria-label="Expand image" onclick={() => viewer = block}><canvas use:pixels={block} aria-label="Image attachment" style:aspect-ratio={`${block.width} / ${block.height}`}></canvas></button>
      {:else if block.kind === "math"}
        <MathFormula tex={block.tex} source={block.source} preview={block.preview ?? block.source} display={true} />
      {:else if block.role === "rule"}
        <hr>
      {:else}
        <div class="text" class:quote={block.role === "quote"} class:heading={typeof block.role === "object" && block.role?.heading} class:list-item={typeof block.role === "object" && block.role?.item}>
          {#if block.role?.item}<span class="list-marker">{block.role.item.marker}</span>{/if}{#each textRuns(block) as run}{#if run.math && !run.style.code && !run.link}<MathFormula tex={run.math} source={run.text} />{:else if run.link}<a href={run.link} target="_blank" rel="noopener noreferrer" referrerpolicy="no-referrer">{run.text}</a>{:else}<span class:bold={run.style.bold} class:italic={run.style.italic} class:underline={run.style.underline} class:strike={run.style.strike} class:inline-code={run.style.code}>{run.text}</span>{/if}{/each}
        </div>
      {/if}
    {/each}
    {#if shown.hasMore || expanded}<button class="more" onclick={() => expanded = !expanded} aria-expanded={expanded}>{expanded ? "Show less" : "Show more"}</button>{/if}
    {#if copied}<small class="copy-status" role="status">{copied}</small>{/if}
</div>
{#if viewer}
  <dialog class="image-viewer" use:dialog onclose={() => viewer = null} oncancel={() => viewer = null} aria-label="Image attachment">
    <header><span>Image attachment</span><button aria-label="Close image" onclick={() => viewer = null}>×</button></header>
    <canvas use:pixels={viewer} aria-label="Expanded image attachment"></canvas>
  </dialog>
{/if}
