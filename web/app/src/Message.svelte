<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { visibleBlocks, codeColor } from "./presentation.js";
  let { message } = $props();
  let expanded = $state(false);
  const shown = $derived(visibleBlocks(message.document, expanded));
</script>
<article class:outgoing={message.outgoing}>
  <header><strong>{message.author}</strong><time datetime={new Date(message.at_ms).toISOString()}>{new Date(message.at_ms).toLocaleTimeString([], {hour:"2-digit", minute:"2-digit"})}</time>{#if message.status}<span>{message.status}</span>{/if}</header>
  {#each shown.blocks as block}
    {#if block.kind === "code"}
      <pre aria-label={block.language ? `${block.language} code` : "Code"}><code>{#each block.runs as run}<span style:color={codeColor(run.foreground)} style:font-weight={run.bold ? "bold" : "normal"} style:font-style={run.italic ? "italic" : "normal"}>{run.text}</span>{/each}</code></pre>
    {:else}
      <div class="text">{block.text}</div>
    {/if}
  {/each}
  {#if shown.hasMore || expanded}<button class="more" onclick={() => expanded = !expanded} aria-expanded={expanded}>{expanded ? "Show less" : "Show more"}</button>{/if}
</article>
<style>
article { padding: .6rem 1rem; overflow-wrap: anywhere; }
article:hover { background: color-mix(in srgb, var(--text) 3%, transparent); }
header { display:flex; align-items:baseline; gap:.65rem; font-size:.9rem; }
header time, header span { color:var(--muted); font-size:.75rem; }
.text { white-space:pre-wrap; }
pre { white-space:pre; overflow:auto; background:#18202c; color:#d8dee9; padding:.75rem; border-radius:.4rem; font-size:.85rem; }
.more { border:0; color:var(--accent); background:none; padding:.25rem 0; cursor:pointer; font:inherit; }
</style>
