<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { visibleBlocks, documentRows, codeColor, textRuns, rgbaPixels } from "./presentation.js";
  import MathFormula from "./MathFormula.svelte";
  import { emoji } from "./emoji-state.svelte.js";
  import { emojiParts, imageUrl } from "./emoji.js";
  const base = import.meta.env.BASE_URL;
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
<!-- Emoji drawn as Twemoji: the characters stay text (transparent, over the
     image as a background), so selecting, copying, find and screen readers
     get them in every browser. Code keeps its characters. -->
{#snippet withEmoji(value)}{#each emojiParts(emoji.index, value) as part}{#if part.key}<span class="emoji" style:background-image={`url("${imageUrl(base, part.key)}")`}>{part.text}</span>{:else}{part.text}{/if}{/each}{/snippet}
{#snippet one(block)}
      {#if block.kind === "code"}
        <div class="code-block"><div class="code-toolbar"><span>{block.language || "Code"}</span><button onclick={() => copyCode(block)} aria-label="Copy code">Copy</button></div><pre aria-label={block.language ? `${block.language} code` : "Code"}><code>{#each block.runs as run}<span style:color={codeColor(run.foreground)} style:font-weight={run.bold ? "bold" : "normal"} style:font-style={run.italic ? "italic" : "normal"}>{run.text}</span>{/each}</code></pre></div>
      {:else if block.kind === "image"}
        <button class="image-button" aria-label="Expand image" onclick={() => viewer = block}><canvas use:pixels={block} aria-label="Image attachment" style:aspect-ratio={`${block.width} / ${block.height}`}></canvas></button>
      {:else if block.kind === "math"}
        <MathFormula tex={block.tex} source={block.source} preview={block.preview ?? block.source} display={true} />
      {:else if block.role === "rule"}
        <div class="rule"><hr></div>
      {:else}
        <div class="text" class:quote={block.role === "quote"} class:heading={typeof block.role === "object" && block.role?.heading} class:list-item={typeof block.role === "object" && block.role?.item}>
          {#if block.role?.item}<span class="list-marker">{block.role.item.marker}</span>{/if}{#each textRuns(block) as run}{#if run.math && !run.style.code && !run.link}<MathFormula tex={run.math} source={run.text} />{:else if run.link}<a class:bold={run.style.bold} class:italic={run.style.italic} class:underline={run.style.underline} class:strike={run.style.strike} href={run.link} target="_blank" rel="noopener noreferrer" referrerpolicy="no-referrer">{@render withEmoji(run.text)}</a>{:else}<span class:bold={run.style.bold} class:italic={run.style.italic} class:underline={run.style.underline} class:strike={run.style.strike} class:inline-code={run.style.code}>{#if run.style.code}{run.text}{:else}{@render withEmoji(run.text)}{/if}</span>{/if}{/each}
        </div>
      {/if}
{/snippet}
<div class="typed-document">
    {#each documentRows(shown.blocks) as row}
      {#if row.kind === "break"}
        <!-- Its blank lines as one text node of line breaks, so a selection
             copies them; at most documentRows' clamp per break, and the
             document's break budget in all. -->
        <div class="break" style:--lines={row.lines} aria-hidden="true">{"\n".repeat(row.lines)}</div>
      {:else if row.blocks.length > 1}
        <div class="joined">{#each row.blocks as block}{@render one(block)}{/each}</div>
      {:else}
        {@render one(row.blocks[0])}
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
