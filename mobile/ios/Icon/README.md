# App icon

`icon.svg` is the source. To regenerate the 1024×1024 asset after editing it:

```sh
cd mobile/ios/Icon
qlmanage -t -s 1024 -o . icon.svg            # renders icon.svg.png (has alpha)
swiftc -O -o flatten flatten.swift
./flatten icon.svg.png ../Pigeons/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png
rm -f icon.svg.png flatten
```

App Store icons must be opaque; `flatten.swift` draws the render onto a solid
background and writes an RGB PNG without an alpha channel.
