<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { initials, avatarColor } from "./presentation.js";
  import TypedDocument from "./TypedDocument.svelte";
  import {documents,contentDocument,retainDocument,contentFailed,contentUnavailable,retryContent} from "./content-bridge.js";
  let { message } = $props();
  const content = $derived.by(() => { $documents; return contentDocument(message.content_token) ?? message.document; });
  const failed = $derived.by(() => { $documents; return message.content_failed || contentFailed(message.content_token); });
  const unavailable = $derived.by(() => { $documents; return contentUnavailable(); });
  $effect(() => retainDocument(message.content_token,message.content_failed));
  const date = $derived(new Date(Number(message.at_ms)));
</script>
<article class:outgoing={message.outgoing}>
  <div class={`avatar ${avatarColor(message.author)}`} aria-hidden="true">{initials(message.author)}</div>
  <div class="message-content">
    <header><strong>{message.author}</strong><time datetime={Number.isNaN(date.getTime()) ? undefined : date.toISOString()} title={date.toLocaleString()}>{date.toLocaleTimeString([], {hour:"2-digit", minute:"2-digit"})}</time>{#if message.status}<span class:failed={message.status.toLowerCase().includes("fail")}>{message.status}</span>{/if}</header>
    <TypedDocument document={content} />
    {#if failed}
      {#if unavailable}<small role="status">Preview unavailable. Reload to try again.</small>
      {:else}<button onclick={() => retryContent(message.content_token)}>Retry message preview</button>{/if}
    {/if}
  </div>
</article>
