# MJPEG
## Server
### fast_image_resize + image-rs JPEG encoder
Around 3ms per frame at 400x240 and 50% quality.
Each frame is around 5-15KB at 400x240 and 50% quality.
Each frame is around 5-25KB at 400x240 and 80% quality.

## Client
### TCP frame read latency
Around 10-50ms per frame, with outliers up to 200ms.

### jpeg-decoder
Around 140-155ms per frame at 400x240 and 50% quality.

### zune-jpeg
Around 65-90ms per frame at 400x240 and 80% quality. Usually 70ms.

### Raw framebuffer copy (after decoding)
Around 2.8-4ms per frame.