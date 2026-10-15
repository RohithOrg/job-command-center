#!/bin/bash
# Build the Linux installers (.deb and .AppImage, x86-64) inside a Docker container, from any OS.
# Output: dist/. Build caches live in Docker volumes, not in this folder.
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm --platform linux/amd64 \
  -v "$PWD":/src -w /src \
  -v jcc-cargo-registry:/usr/local/cargo/registry \
  -v jcc-linux-target:/target -e CARGO_TARGET_DIR=/target \
  rust:1-bookworm bash -euc '
    apt-get update -qq
    apt-get install -y -qq libxkbcommon-dev libfontconfig1-dev libdbus-1-dev libwayland-dev libxcursor-dev \
      libxrandr-dev libxi-dev libgl1-mesa-dev cmake clang file >/dev/null
    command -v cargo-packager >/dev/null || cargo install cargo-packager --locked -q
    cargo packager --release --formats deb
    mkdir -p dist
    find /target -maxdepth 3 -name "*.deb" -exec cp {} dist/ \;
    # AppImage, assembled by hand: linuxdeploy crashes under emulation on Apple silicon, and the app
    # only needs libraries every desktop Linux has (X11/Wayland, fontconfig).
    app=/tmp/AppDir; rm -rf $app; mkdir -p $app/usr/bin
    cp /target/release/job-command-center $app/usr/bin/
    cp assets/icon-512.png $app/job-command-center.png
    printf "[Desktop Entry]\nType=Application\nName=Job Command Center\nExec=job-command-center\nIcon=job-command-center\nCategories=Office;\n" > $app/job-command-center.desktop
    printf "#!/bin/sh\nexec \"\$(dirname \"\$(readlink -f \"\$0\")\")/usr/bin/job-command-center\" \"\$@\"\n" > $app/AppRun; chmod +x $app/AppRun
    if [ ! -x /target/appimagetool/AppRun ]; then
      curl -sSL -o /tmp/appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
      # Blank the AppImage magic bytes so emulators (Docker on Apple silicon) run it as a plain ELF.
      printf "\0\0\0" | dd of=/tmp/appimagetool bs=1 seek=8 count=3 conv=notrunc 2>/dev/null
      chmod +x /tmp/appimagetool && (cd /tmp && ./appimagetool --appimage-extract >/dev/null)
      rm -rf /target/appimagetool && mv /tmp/squashfs-root /target/appimagetool
    fi
    ARCH=x86_64 /target/appimagetool/AppRun --no-appstream $app dist/Job-Command-Center-0.1.0-x86_64.AppImage
    ls -la dist'
