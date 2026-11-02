# hevctui

App desktop (Rust, egui) che ricomprime in HEVC una libreria media (anche su NAS/share di rete) usando l'encoder NVENC della GPU NVIDIA.

Salta i file già HEVC/AV1 e quelli sotto la soglia di bitrate: ricomprimere file già efficienti non fa risparmiare. Prima di ogni encode può stimare il risparmio su pochi spezzoni e tenere l'originale se non conviene.

- **Uso:** `hevctui.exe` apre la GUI; `hevctui.exe scan` da terminale aggiorna l'indice e stampa il riepilogo senza GUI.
- **Impostazioni:** tutte dalla GUI (⚙ Impostazioni): cartelle da scansionare (Film/Serie), parametri NVENC, soglie, stima a campioni, scansione, percorsi di ffmpeg/ffprobe. Al primo avvio la lista cartelle è vuota.
- **ffmpeg/ffprobe:** per default cercati nel `PATH`; in alternativa si indica il percorso completo dalla GUI. Serve una build con `hevc_nvenc`.
- **Dati:** `%LOCALAPPDATA%\hevctui\` (`config.toml`, `library.json`, `hevctui.log`, `samples\`). I config del formato precedente (`root` + sottocartelle) vengono migrati automaticamente.
- **Sorgente:** `src/core.rs` (motore: config, scan, stima a campioni, coda NVENC, stato) + `src/main.rs` (GUI).

## Build

Toolchain rustup GNU (`stable-x86_64-pc-windows-gnu`) + mingw-w64 (es. WinLibs POSIX/MSVCRT) nel `PATH` per `as`/`dlltool`.

```powershell
cargo build --release
```

`eframe` è fissato a `=0.36.1` con renderer **wgpu**: con glow la finestra può restare bianca su PC con doppia GPU.
