<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  let { name, size = 20 } = $props();
  const paths = {
    play: "M8 5v14l11-7z",
    pause: "M6 5h4v14H6zM14 5h4v14h-4z",
    skip: "M5 6l8.5 6L5 18zM15 6h2.5v12H15z",
    add: "M11 5h2v6h6v2h-6v6h-2v-6H5v-2h6z",
    next: "M3 6h12v2H3zM3 10h12v2H3zM3 14h8v2H3zM16 13v7l5.5-3.5z",
    remove: "M6.4 5L12 10.6 17.6 5 19 6.4 13.4 12 19 17.6 17.6 19 12 13.4 6.4 19 5 17.6 10.6 12 5 6.4z",
    grip: "M9 5h2v2H9zM13 5h2v2h-2zM9 11h2v2H9zM13 11h2v2h-2zM9 17h2v2H9zM13 17h2v2h-2z",
    volume: "M4 9h4l5-4v14l-5-4H4zM15.5 8.1l1.4-1.4a7.5 7.5 0 0 1 0 10.6l-1.4-1.4a5.5 5.5 0 0 0 0-7.8z",
    duck: "M4 4h16v12H8l-4 4zM8 8h8v2H8zM8 11h5v2H8z",
    queue: "M3 6h18v2H3zM3 11h18v2H3zM3 16h12v2H3z",
    album: "M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 7a3 3 0 1 1 0 6 3 3 0 0 1 0-6z",
    artist: "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zm-8 8c0-3.3 3.6-6 8-6s8 2.7 8 6v1H4z",
    playlist: "M3 5h13v2H3zM3 9h13v2H3zM3 13h9v2H3zM18 5h3v2h-1v9.5a2.5 2.5 0 1 1-2-2.45z",
    channel: "M12 3a8 8 0 0 0-8 8v6a3 3 0 0 0 3 3h2v-7H6v-2a6 6 0 0 1 12 0v2h-3v7h2a3 3 0 0 0 3-3v-6a8 8 0 0 0-8-8z",
    search: "M10 3a7 7 0 1 0 4.2 12.6l5.1 5.1 1.4-1.4-5.1-5.1A7 7 0 0 0 10 3zm0 2a5 5 0 1 1 0 10 5 5 0 0 1 0-10z",
    menu: "M3 6h18v2H3zM3 11h18v2H3zM3 16h18v2H3z",
    close: "M6.4 5L12 10.6 17.6 5 19 6.4 13.4 12 19 17.6 17.6 19 12 13.4 6.4 19 5 17.6 10.6 12 5 6.4z",
    note: "M12 3v10.55A4 4 0 1 0 14 17V7h4V3z",
    similar: "M7 18h2V6H7zM11 22h2V2h-2zM3 14h2v-4H3zM15 18h2V6h-2zM19 10v4h2v-4z",
    path: "M16 6l2.29 2.29-4.88 4.88-4-4L2 16.59 3.41 18l6-6 4 4 6.3-6.29L22 12V6z",
    link: "M3.9 12c0-1.71 1.39-3.1 3.1-3.1h4V7H7c-2.76 0-5 2.24-5 5s2.24 5 5 5h4v-1.9H7c-1.71 0-3.1-1.39-3.1-3.1zM8 13h8v-2H8zm9-6h-4v1.9h4c1.71 0 3.1 1.39 3.1 3.1s-1.39 3.1-3.1 3.1h-4V17h4c2.76 0 5-2.24 5-5s-2.24-5-5-5z",
    headphones: "M12 3a9 9 0 0 0-9 9v7a2 2 0 0 0 2 2h3v-8H5v-1a7 7 0 0 1 14 0v1h-3v8h3a2 2 0 0 0 2-2v-7a9 9 0 0 0-9-9z",
    expand: "M21 3v8l-3.29-3.29-10 10L11 21H3v-8l3.29 3.29 10-10L13 3z",
    fullscreen: "M5 5h6v2H7v4H5zM13 5h6v6h-2V7h-4zM5 13h2v4h4v2H5zM17 13h2v6h-6v-2h4z",
    "fullscreen-exit": "M9 5h2v6H5V9h4zM13 5h2v4h4v2h-6zM5 13h6v6H9v-4H5zM13 13h6v2h-4v4h-2z",
  };
</script>

<svg viewBox="0 0 24 24" width={size} height={size} fill="currentColor" fill-rule="evenodd" aria-hidden="true" focusable="false">
  <path d={paths[name]} />
</svg>

<style>
  svg { flex: none; display: block; }
</style>
