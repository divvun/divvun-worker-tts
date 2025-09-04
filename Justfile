tmp := `mktemp -d`
pwd := `pwd`


build-linux:
    mkdir -p {{tmp}}/lib
    LZMA_API_STATIC=1 \
        TMP_PATH={{tmp}} \
        LIBTORCH=/usr \
        LIBTORCH_BYPASS_VERSION_CHECK=1 \
        cargo build --release
    # patchelf --set-rpath /opt/libtorch/lib ./target/x86_64-unknown-linux-gnu/release/divvun-worker-tts
    rm -rf {{tmp}}


build-macos features="mp3":
    # Workaround for macOS eagerly linking dylibs no matter what we tell it
    # mkdir -p {{tmp}}/lib
    # ln -s /opt/homebrew/opt/icu4c/lib/*.a {{tmp}}/lib
    # ln -s /opt/libtorch/lib/*.a {{tmp}}/lib
    LZMA_API_STATIC=1 \
        LIBTORCH=/opt/homebrew \
        LIBTORCH_BYPASS_VERSION_CHECK=1 \
        cargo build --release --features {{features}}
    install_name_tool -add_rpath /opt/homebrew/lib ./target/release/divvun-worker-tts
    rm -rf {{tmp}}

build-docker:
    docker buildx build --platform linux/amd64 -t ghcr.io/divvun/divvun-worker-tts:latest .

push-docker:
    docker push ghcr.io/divvun/divvun-worker-tts:latest

build-and-push-docker: build-docker push-docker

# build-lib-macos-aarch64:
#     # Workaround for macOS eagerly linking dylibs no matter what we tell it
#     mkdir -p {{tmp}}/lib
#     cp -r /opt/homebrew/opt/icu4c/lib/*.a {{tmp}}/lib
#     @ARTIFACT_PATH=/opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11 \
#         TMP_PATH={{tmp}} \
#         LZMA_API_STATIC=1 \
#         PYO3_CONFIG_FILE={{pwd}}/pyo3-mac.txt \
#         cargo build -p divvun-runtime --lib --no-default-features --release \
#         --features ffi,divvun-runtime/mod-cg3,divvun-runtime/mod-hfst,divvun-runtime/mod-divvun
#     @install_name_tool \
#         -change /opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11/Python \
#         @loader_path/libpython3.11.dylib \
#         ./target/release/libdivvun_runtime.dylib
#     @install_name_tool -change \
#         /opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11/Python \
#         @loader_path/libpython3.11.dylib ./target/release/libpython3.11.dylib
#     @rm -rf {{tmp}}

# build-lib-macos-swift-aarch64:
#     @CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_DEBUG=true RUST_BACKTRACE=1 ARTIFACT_PATH=/opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11 \
#         PYO3_CONFIG_FILE={{pwd}}/pyo3-mac.txt \
#         cargo build -p divvun-runtime --lib --no-default-features --features swift \
#         --target aarch64-apple-darwin --release \
#         --features divvun-runtime/mod-cg3,divvun-runtime/mod-hfst,divvun-runtime/mod-divvun -vv
#     # swift-bridge-cli create-package \
#     #     --bridges-dir ./generated \
#     #     --out-dir DivvunRuntime \
#     #     --macos target/aarch64-apple-darwin/release/libdivvun_runtime.a \
#     #     --name DivvunRuntime
