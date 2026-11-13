<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { initials, avatarColor } from "./presentation.js";
  import {messageTime,timestampFormats} from "./message-time.js";
  import TypedDocument from "./TypedDocument.svelte";
  import {observeDocument,contentDocument,retainDocument,contentFailed,contentUnavailable,retryContent} from "./content-bridge.js";
  let { message } = $props();
  const token = $derived(message.content_token);
  const serverFailed = $derived(message.content_failed);
  let prepared = $state(null), cacheFailed = $state(false), unavailable = $state(false);
  const content = $derived(prepared ?? message.document);
  const failed = $derived(serverFailed || cacheFailed);
  $effect(() => {
    const current=token;
    const stop=observeDocument(current,()=>{
      prepared=contentDocument(current);cacheFailed=contentFailed(current);unavailable=contentUnavailable();
    });
    const release=retainDocument(current,serverFailed);
    return()=>{stop();release();};
  });
  const timestamp = $derived(message.at_ms);
  const time = $derived.by(() => { $timestampFormats; return messageTime(timestamp); });
</script>
<article class:outgoing={message.outgoing}>
  <div class={`avatar ${avatarColor(message.author)}`} aria-hidden="true">{initials(message.author)}</div>
  <div class="message-content">
    <header><strong>{message.author}</strong><time datetime={time.iso} title={time.title}>{time.label}</time>{#if message.status}<span class:failed={message.status.toLowerCase().includes("fail")}>{message.status}</span>{/if}</header>
    <TypedDocument document={content} />
    {#if failed}
      {#if unavailable}<small role="status">Preview unavailable. Reload to try again.</small>
      {:else}<button onclick={() => retryContent(message.content_token)}>Retry message preview</button>{/if}
    {/if}
  </div>
</article>
