# Auto Renamer

**Beta.** Version 1.0.0 is an early public release. It works, and details can still change.

Auto Renamer watches one folder on Windows and renames each new image into a numbered series, such as `screenshot_00001.png`.

It leaves files that are already in the folder alone, and it does not look inside subfolders. A file is renamed once, after it has finished writing.

## Use

1. Choose a source folder and a filename.
2. The next file saved there becomes `{filename}_{number}.{ext}`. The number continues from the highest match already in the destination.
3. **Reset ID** starts again at the first free number from 1.
4. **ID digits** sets how many digits the number is padded to, from 1 to 10. The default is 5.
5. **Copy to output folder** keeps the original and writes the new file into another folder. With it off, the file is renamed where it was saved.
6. **Auto convert** writes JPG, PNG, WEBP, AVIF, or BMP. Quality runs from 1 to 100 and applies to JPG, WEBP, and AVIF. A file that is already in the chosen format is not converted.
7. **Resize images** scales to the given size. Stretch forces the exact dimensions.

Watched types are png, jpg, jpeg, webp, avif, gif, bmp, tif, tiff, heic, heif, jxl, and tga. Names that start with `.` or `~` are skipped.

Convert and resize use FFmpeg. The first time either is turned on, Auto Renamer downloads it. If that download fails, files are still renamed.

Close and minimize send the window to the notification area. Quit is on the tray menu. A second start brings the window that is already running back up.

**Start with Windows** registers the current program for the signed-in user. It does not need administrator rights. Turn it off to start Auto Renamer yourself.

On startup, Auto Renamer checks for a newer release and asks before updating. The GitHub icon in the footer opens the project page.

Settings are stored in `%APPDATA%\Auto Renamer\config.json`.

## Download

Windows x64. A tagged release includes both files:

- `auto-renamer.exe`, which runs on its own
- `AutoRenamer-Setup.exe`, a per-user installer

## Build

Rust stable, on Windows:

```
cargo build --release --locked
```

The program is `target\release\auto-renamer.exe`.

The installer also needs [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```
ISCC.exe /DAppVersion=1.0.0 installer\auto-renamer.iss
```

That writes `target\installer\AutoRenamer-Setup.exe`.

## License

GPL-3.0-only. Copyright (C) 2026 BenZoN. See [LICENSE](LICENSE).
