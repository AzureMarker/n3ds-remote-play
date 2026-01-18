# MJPEG
## Server
### fast_image_resize + image-rs JPEG encoder
Around 3ms per frame at 400x240 and 50% quality.
Each frame is around 5-15KB at 400x240 and 50% quality.
Each frame is around 5-25KB at 400x240 and 80% quality.

### MPEG-1 (ffmpeg)
Around 3ms per frame at 400x240 and 1Mbps.
I-frames are around 15-20KB, P-frames around 1-7KB.

## Client
### TCP frame read latency
Around 10-50ms per frame, with outliers up to 200ms.

### jpeg-decoder
Around 140-155ms per frame at 400x240 and 50% quality.

### zune-jpeg
Around 65-90ms per frame at 400x240 and 80% quality. Usually 70ms.

### MPEG-1 (ffmpeg)
Around 17-30ms per frame at 400x240 and 1Mbps.
I-frames are around 24-30ms, P-frames around 17-20ms.

### Raw framebuffer copy (after decoding)
Around 2.8-4ms per frame.