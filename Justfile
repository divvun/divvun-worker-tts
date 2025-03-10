tmp := `mktemp -d`
pwd := `pwd`

build:
    cargo build --release --no-default-features \
        --features divvun-runtime/mod-speech,divvun-runtime/mod-ssml,divvun-runtime/mod-cg3,divvun-runtime/mod-hfst,divvun-runtime/mod-divvun

build-linux:
    # {{pwd}}/linux/libtorch
    export ARTIFACT_PATH=/usr
    # export LZMA_API_STATIC=1
    export TMP_PATH={{tmp}}
    # export LIBTORCH={{pwd}}/linux/libtorch/lib
    # export LIBTORCH_BYPASS_VERSION_CHECK=1
    cross build --target x86_64-unknown-linux-gnu --release --no-default-features \
        --features divvun-runtime/mod-speech,divvun-runtime/mod-ssml,divvun-runtime/mod-cg3,divvun-runtime/mod-hfst,divvun-runtime/mod-divvun

build-macos:
    # Workaround for macOS eagerly linking dylibs no matter what we tell it
    mkdir -p {{tmp}}/lib
    cp -r /opt/homebrew/opt/icu4c/lib/*.a {{tmp}}/lib
    @ARTIFACT_PATH=/opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11 \
        LZMA_API_STATIC=1 \
        TMP_PATH={{tmp}} \
        PYO3_CONFIG_FILE={{pwd}}/pyo3-mac.txt \
        cargo build --release
    @install_name_tool -change /opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11/Python @loader_path/libpython3.11.dylib ./target/release/divvun-worker-tts
    cp /opt/homebrew/opt/python@3.11/Frameworks/Python.framework/Versions/3.11/Python ./target/release/libpython3.11.dylib
    @rm -rf {{tmp}}

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
