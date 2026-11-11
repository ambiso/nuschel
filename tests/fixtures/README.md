# WebRTC video fixture

`rtc-keyframe.vp8` is a single 160x120 VP8 keyframe, generated locally from FFmpeg's
`testsrc2` pattern. It contains no camera capture or third-party video. It is part of
Nuschl's EUPL-1.2 test material.

Generation:

```sh
ffmpeg -f lavfi -i testsrc2=size=160x120:rate=1 -frames:v 1 \
  -c:v libvpx -b:v 800k -threads 2 -f ivf fixture.ivf
```

Extract the first frame after the 32-byte IVF header and 12-byte frame header. The
transport tests compare the entire reassembled payload. FFmpeg can decode the original
IVF to validate the fixture; neither the application nor the Nix build needs FFmpeg.
This establishes encoded-frame transport, not live capture, rendering or A/V sync.
