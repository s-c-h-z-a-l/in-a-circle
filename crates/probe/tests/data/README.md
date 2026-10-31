# JPEG fixtures

Tiny JPEGs for the probe and PDF tests. Sources are generated with
ImageMagick 7 (`magick`), then encoded with libjpeg-turbo `cjpeg`
(`magick`'s own encoder for the CMYK file) and Pillow 12.3. Run in this
directory; `$T` is any scratch directory.

```sh
magick -size 24x16 gradient:white-black -colorspace Gray -depth 8 $T/gray.pgm
magick -size 32x24 gradient:red-blue -depth 8 $T/rgb.ppm
magick -size 40x30 gradient:yellow-green -depth 8 $T/prog.ppm
magick -size 16x16 xc:red -fill blue -draw "rectangle 8,0 15,15" -depth 8 $T/cmyk_src.ppm
magick -size 20x10 gradient:cyan-magenta -depth 8 $T/exif.ppm

cjpeg -quality 90 -grayscale -outfile gray.jpg $T/gray.pgm
cjpeg -quality 90 -sample 1x1 -baseline -outfile rgb.jpg $T/rgb.ppm
cjpeg -quality 90 -sample 1x1 -progressive -outfile progressive.jpg $T/prog.ppm
magick $T/cmyk_src.ppm -colorspace CMYK -sampling-factor 1x1 -quality 90 cmyk_adobe.jpg
python3 -I exif.py $T/exif.ppm exif_orientation6.jpg
```

`exif.py` (not kept; three lines):

```python
import sys
from PIL import Image
im = Image.open(sys.argv[1]); ex = Image.Exif(); ex[0x0112] = 6
im.save(sys.argv[2], "JPEG", quality=90, subsampling=0, exif=ex)
```

| file | size | what |
|---|---|---|
| gray.jpg | 24x16 | 1 component, baseline |
| rgb.jpg | 32x24 | 3 components, baseline, 4:4:4 |
| progressive.jpg | 40x30 | 3 components, progressive (SOF2) |
| cmyk_adobe.jpg | 16x16 | 4 components, Adobe APP14, inverted values; left half red, right half blue |
| exif_orientation6.jpg | 20x10 | APP1 Exif, Orientation = 6 |
