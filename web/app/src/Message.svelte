<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { initials, avatarColor } from "./presentation.js";
  import TypedDocument from "./TypedDocument.svelte";
  import {documents,contentDocument,retainDocument,contentFailed,retryContent} from "./content-bridge.js";
  let { message } = $props();
  const content = $derived.by(() => { $documents; return contentDocument(message.content_token) ?? message.document; });
  const failed = $derived.by(() => { $documents; return contentFailed(message.content_token); });
  $effect(() => retainDocument(message.content_token));
  const date = $derived(new Date(Number(message.at_ms)));
</script>
<article class:outgoing={message.outgoing}>
  <div class={`avatar ${avatarColor(message.author)}`} aria-hidden="true">{initials(message.author)}</div>
  <div class="message-content">
    <header><strong>{message.author}</strong><time datetime={Number.isNaN(date.getTime()) ? undefined : date.toISOString()} title={date.toLocaleString()}>{date.toLocaleTimeString([], {hour:"2-digit", minute:"2-digit"})}</time>{#if message.status}<span class:failed={message.status.toLowerCase().includes("fail")}>{message.status}</span>{/if}</header>
    <TypedDocument document={content} />
    {#if failed}<button onclick={() => retryContent(message.content_token)}>Retry message preview</button>{/if}
  </div>
</article>
