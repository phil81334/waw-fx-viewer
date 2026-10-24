# Attribution

Created by **phil81334** for the World at War modding community.

- EFX parsing and visual playback algorithms are adapted from [iw3xo-radiant](https://github.com/xoxor4d/iw3xo-radiant), by xoxor4d and contributors (MIT, revision `665347c2e90671068088ddc4cd58291194cf9423`), with WaW-specific checks against the official tools. Its upstream notice is retained below.
- Wavelet IWI decoding and codewords are adapted from [OpenAssetTools](https://github.com/Laupetin/OpenAssetTools), by Laupetin and contributors (GPL-3.0, revision `30d2d498bd8e3f2401037b24f3db6f96d39554c4`), through [WaW Image Viewer](https://github.com/phil81334/waw-image-viewer).
- Loose-model reading builds on [WaW Model Viewer](https://github.com/phil81334/waw-model-viewer), including its [cod-asset-importer](https://github.com/mauserzjeh/cod-asset-importer) format reference by mauserzjeh and contributors (GPL-3.0, revision `b67ceebcc73036179f1da37aecdc0fbba6cb17bb`). Model coordinates, UVs, colours, bones and native normal/vector packing were checked against the official WaW tools.
- Built with Tauri, Vite, Microsoft's Rust Windows bindings, the Rust image and zip libraries, and their dependencies. Versions are recorded in the lockfiles.

The combined app is distributed under [GPL-3.0](LICENCE). It contains no game executables, effects, models, textures or shaders. Assets read from your installation remain the property of their respective owners.

This is an independent community app, not an official Treyarch, Activision or UGX product.

## iw3xo-radiant upstream MIT notice

```text
MIT License 
Copyright (c) 2020-2023 xoxor4d <xoxor4d@outlook.com>

https://github.com/Nukem9/detours
Copyright (c) 2019 Nukem <Nukem@outlook.com>

https://github.com/ocornut/imgui
Copyright (c) 2014-2022 Omar Cornut

https://github.com/nlohmann/fifo_map
Copyright (c) 2015-2017 Niels Lohmann

https://gist.github.com/maluoi/ade07688e741ab188841223b8ffeed22
Copyright (c) 2019 Nick Klingensmith (@koujaku)

https://github.com/ocornut/imgui/issues/1901
Copyright (c) 2021 zfedoran

https://github.com/patrickcjk/imgui-notify
Copyright (c) 2021 Patrick


Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
