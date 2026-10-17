{
    lib,
    stdenv,
    rustPlatform,
    pkg-config,
    cmake,
    nasm,
    makeWrapper,
    swift,
    swiftpm,
    swiftPackages,
    apple-sdk_15,
    wrapGAppsHook4,
    autoPatchelfHook,
    glib,
    glib-networking,
    gsettings-desktop-schemas,
    gtk4,
    webkitgtk_6_0,
    cairo,
    pango,
    gdk-pixbuf,
    graphene,
    libsoup_3,
    fontconfig,
    libxkbcommon,
    wayland,
    vulkan-loader,
    libGL,
    libGLX,
    libpulseaudio,
    libglvnd,
    alsa-lib,
    gst_all_1,
    pipewire,
    libX11,
    libXi,
    libXrandr,
    libXcursor,
    bubblewrap,
    xdg-dbus-proxy,
}: let
    inherit (stdenv.hostPlatform) isLinux isDarwin;

    # webKit stack
    toolkitDeps = [
        glib
        glib-networking
        gsettings-desktop-schemas
        gtk4
        webkitgtk_6_0
        cairo
        pango
        gdk-pixbuf
        graphene
        libsoup_3
        fontconfig
    ];

    graphicsDeps = [
        vulkan-loader
        libGL
        libGLX
        libglvnd
    ];

    windowingDeps = [
        wayland
        libxkbcommon
        libX11
        libXi
        libXrandr
        libXcursor
    ];

    audioDeps = [
        libpulseaudio
        alsa-lib
    ];

    gstPlugins = with gst_all_1; [
        gst-plugins-base
        gst-plugins-good
        gst-plugins-bad
        gst-libav
        gstreamer
        pipewire
    ];

    runtimeTools = [
        bubblewrap
        xdg-dbus-proxy
    ];
in
    rustPlatform.buildRustPackage (finalAttrs: {
        pname = "leech";
        # Match `cargo build`: release automation versions Cargo.toml, not this file.
        version = (lib.importTOML ../Cargo.toml).workspace.package.version;

        src = ../.;

        cargoLock = {
            lockFile = "${finalAttrs.src}/Cargo.lock";
            allowBuiltinFetchGit = true;
        };

        cargoBuildFlags = [
            "--package"
            "leech"
        ];

        nativeBuildInputs =
            [
                pkg-config
                cmake
                makeWrapper
            ]
            ++ lib.optionals stdenv.hostPlatform.isx86 [
                nasm
            ]
            ++ lib.optionals isLinux [
                wrapGAppsHook4
                autoPatchelfHook
            ]
            ++ lib.optionals isDarwin [
                swift
                swiftpm
            ];

        buildInputs =
            lib.optionals isLinux (
                toolkitDeps ++ graphicsDeps ++ windowingDeps ++ audioDeps ++ gstPlugins
            )
            ++ lib.optionals isDarwin [
                apple-sdk_15
                swiftPackages.stdlib
            ];

        # winit loads these libraries dynamically; retain them in the runtime RPATH.
        runtimeDependencies = lib.optionals isLinux (graphicsDeps ++ windowingDeps);

        dontUseSwiftpmBuild = true;
        dontUseSwiftpmCheck = true;
        dontUseSwiftpmInstall = true;
        doCheck = false;

        # Swift 6 split the stdlib out of the toolchain and SDK: the final Rust
        # link needs the SDK overlays plus the store stdlib/toolchain libs.
        preBuild = lib.optionalString isDarwin ''
            export RUSTFLAGS="$RUSTFLAGS -L native=$SDKROOT/usr/lib/swift -L native=${lib.getLib swiftPackages.stdlib}/lib -L native=${lib.getDev swiftPackages.stdlib}/lib -L native=${lib.getLib swift}/lib"
        '';

        preFixup = lib.optionalString isLinux ''
            gappsWrapperArgs+=(
              --prefix PATH : "${lib.makeBinPath runtimeTools}"
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${
                lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins
            }"
            )
        '';

        # Mirror `cargo xtask package`: notices, licenses and the corresponding
        # MPL-2.0 hpke-rs source ship with every binary.
        postInstall =
            ''
                docs="$out/share/doc/leech"
            ''
            + lib.optionalString isDarwin ''
                app="$out/Applications/Leech.app/Contents"
                docs="$app/Resources"
            ''
            + ''
                mkdir -p "$docs/licenses" "$docs/source"
                cp README.md LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.md "$docs/"
                cp -R assets/licenses/. "$docs/licenses/"
                cp assets/fonts/*-OFL.txt assets/fonts/*-LICENSE.txt "$docs/licenses/"
                cp assets/sounds/README.md "$docs/licenses/notification-sounds.md"
                cp assets/twemoji/LICENSE-GRAPHICS "$docs/licenses/Twemoji-CC-BY-4.0.txt"
                cp assets/twemoji/LICENSE-UNICODE "$docs/licenses/Unicode-LICENSE.txt"
                cp assets/icons/LICENSE "$docs/licenses/Phosphor-Icons-MIT.txt"
                cp assets/icons/LICENSE-SIMPLE-ICONS "$docs/licenses/Simple-Icons-CC0.txt"
                cp -R vendor/hpke-rs "$docs/source/"
            ''
            + lib.optionalString isLinux ''
                install -Dm444 packaging/linux/leech.desktop \
                  $out/share/applications/io.github.rottenvia.leech.desktop
                substituteInPlace $out/share/applications/io.github.rottenvia.leech.desktop \
                  --replace-fail "Exec=leech" "Exec=$out/bin/leech"
                mkdir -p $out/share/icons
                cp -R packaging/linux/hicolor $out/share/icons/
            ''
            # The executable lives inside the bundle so macOS resolves its Info.plist
            # (privacy usage descriptions, identifier and icon) from the launched binary.
            + lib.optionalString isDarwin ''
                install -Dm444 packaging/macos/Info.plist "$app/Info.plist"
                install -Dm444 packaging/macos/Leech.icns "$app/Resources/Leech.icns"
                mkdir -p "$app/MacOS" "$out/share/doc"
                mv "$out/bin/leech" "$app/MacOS/leech"
                ln -s "$app/MacOS/leech" "$out/bin/leech"
                ln -s "$app/Resources" "$out/share/doc/leech"
            '';

        meta = {
            description = "Tiny, performant, native Discord client written in Rust (egui/wgpu)";
            homepage = "https://github.com/ViceVerse-cz/Serein";
            license = with lib.licenses; [
                mit
                asl20
            ];
            platforms = lib.platforms.linux ++ lib.platforms.darwin;
            mainProgram = "leech";
            maintainers = with lib.maintainers; [
                myamusashi
            ];
        };
    })
