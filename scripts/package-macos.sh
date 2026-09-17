#!/bin/sh
# 组装 macOS 发布 tar.gz（roadmap/06 §3.5）。与 scripts/package.sh 的 Linux 包同构：
# 同样的目录形状、同样的 `packaging/` 子目录、同样的命名，换平台的人不必重新认路。
#
#     strixmaid-<版本>-aarch64/
#     ├── strixmaid
#     ├── strixmaid-helper
#     ├── README.md                  安装与卸载说明（packaging/macos/README.md）
#     ├── LICENSE
#     └── packaging/
#         ├── install.sh
#         ├── uninstall.sh
#         ├── io.strixmaid.server.plist
#         ├── io.strixmaid.agent.plist
#         └── pam.d/
#             └── strixmaid.macos
#
# 用法：scripts/package-macos.sh
#
# 只出 **Apple Silicon**（aarch64-apple-darwin），不做 Intel，也不做 universal：
# universal 二进制要把两份代码都塞进去（体积翻倍），而本项目的交付目标里没有
# Intel Mac。需要 Intel 的人自行 `cargo build --release --target x86_64-apple-darwin`。
#
# **暂不做代码签名与公证**。因此用浏览器下载这个包的人会撞上 Gatekeeper 的隔离标记，
# 解法写在 README.md 与 docs/macos-platform.md 里。注意这与下面那步 ad-hoc 签名
# 不是一回事：ad-hoc 只是让二进制在 arm64 上能被执行，不构成任何身份背书。
#
# 依赖：
#   - bun：构建前端（web/dist 不在 git 里）。
#   - Xcode Command Line Tools：链接器 cc 与 codesign / lipo / otool 都来自它。
#   - rustup 的 aarch64-apple-darwin 目标。
# 缺哪一样都直接报错退出并说明装法，不产出半成品。
#
# 兼容性：只用 POSIX sh 语法，能在 macOS 自带的 bash 3.2 下跑。
set -eu

arch="${1:-aarch64}"
case "$arch" in
    aarch64|arm64) ;;
    *)
        echo "本脚本只构建 Apple Silicon（aarch64-apple-darwin）。" >&2
        echo "收到的架构是 ${arch}：Intel 与 universal 都不在交付范围内。" >&2
        exit 2 ;;
esac
target=aarch64-apple-darwin

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

if [ "$(uname -s)" != "Darwin" ]; then
    echo "本脚本必须在 macOS 上运行：链接、codesign、lipo、otool 都要 Apple 的工具链。" >&2
    exit 3
fi

xcode-select -p >/dev/null 2>&1 || {
    echo "缺 Xcode Command Line Tools（链接器 cc 与 codesign 都在里面）：xcode-select --install" >&2
    exit 3
}

if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed | grep -qx "$target" || {
        echo "缺 Rust 目标 ${target}：rustup target add ${target}" >&2
        exit 3
    }
fi

# 前端产物。web/dist 不在 git 里（它是 web/src 的派生物，跟踪必然漂移），
# 所以每次出包都在这里重建一次——本地与 CI 因此走同一条路，
# 不会出现「CI 的包是新的、本地打的包是旧的」。
#
# 顺序不能反：release 下 rust-embed 在【编译期】把 web/dist 嵌进 strixmaid，
# 目录不存在或为空，cargo build 直接失败。
command -v bun >/dev/null 2>&1 || {
    echo "缺 bun：前端产物 web/dist 由 bun 构建，见 https://bun.sh" >&2
    exit 3
}
( cd web && bun install --frozen-lockfile && bun run build )
[ -f web/dist/index.html ] || {
    echo "前端构建未产出 web/dist/index.html" >&2
    exit 3
}

cargo build --release --target "$target" \
    -p strixmaid -p strixmaid-helper

# target 目录可以被 CARGO_TARGET_DIR 或 .cargo/config.toml 改掉，因此问 cargo
# 而不是假定 <repo>/target（package-windows.ps1 出于同样的理由这么做）。
target_dir=$(cargo metadata --no-deps --format-version 1 --manifest-path Cargo.toml \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
[ -n "$target_dir" ] || target_dir="$root/target"
bindir="$target_dir/$target/release"

for bin in strixmaid strixmaid-helper; do
    [ -f "$bindir/$bin" ] || { echo "构建产物缺失：${bindir}/${bin}" >&2; exit 4; }
done

# --------------------------------------------------------------------------
# 产物自检
# --------------------------------------------------------------------------
#
# 三条断言，对应 Linux 侧「不产出动态链接的静态包」那一条的意图：包发出去之前
# 就确认它在别人的机器上能跑。macOS 上没有「全静态」这个选项（libSystem 只有
# 动态版），所以检查的是另外三件事。

for bin in strixmaid strixmaid-helper; do
    f="$bindir/$bin"

    # 1. 架构必须恰好是 arm64。不是 universal（那是另一种交付形态，本项目不做），
    #    也不能因为漏了 --target 而混进一个本机架构的产物。
    archs=$(lipo -archs "$f")
    if [ "$archs" != "arm64" ]; then
        echo "${f} 的架构是「${archs}」，期望恰好是 arm64" >&2
        exit 4
    fi

    # 2. 不许链到 /usr/local 或 /opt/homebrew 下的 dylib。那些是构建机上装的东西，
    #    目标机器没有，装完的表现是「一运行就 dyld: Library not loaded」。
    #    sqlite 由 libsqlite3-sys 编译进二进制，正常情况下这里只该出现
    #    /usr/lib 与 /System/Library 下的系统库。
    # 用 [[:space:]] 而不是 \s：BSD 的 grep -E 不认 \s 这个 GNU/PCRE 扩展。
    if otool -L "$f" | tail -n +2 | grep -E '^[[:space:]]+(/usr/local|/opt/homebrew)' ; then
        echo "${f} 链到了构建机本地的 dylib（见上），目标机器上不存在" >&2
        exit 4
    fi

    # 3. 必须带有效的代码签名。
    #
    #    **这不是「做签名与公证」**，那件事本版本明确不做。arm64 上是另一回事：
    #    内核拒绝执行**完全没有签名**的二进制，所以链接器会自动打一个 ad-hoc
    #    签名（linker-signed）。而本项目的 release profile 开了 strip，strip 会
    #    改动 Mach-O，从而可能让那个签名失效——症状是运行即 Killed: 9，且看不出
    #    任何原因。这里验一遍，失效就用 ad-hoc 重签（codesign -s -，"-" 就是
    #    ad-hoc 的意思，不涉及任何证书）。
    if ! codesign --verify --strict "$f" >/dev/null 2>&1; then
        echo "${f} 的签名无效（多半是 strip 打断了 linker-signed 签名），ad-hoc 重签..."
        codesign --force --sign - "$f"
        codesign --verify --strict "$f" || {
            echo "${f} ad-hoc 重签之后仍验不过，停止出包" >&2
            exit 4
        }
    fi
done

# helper 必须链到系统的 PAM。macOS 自带 OpenPAM，SDK 里有 libpam.tbd，
# 运行期解析到的是 /usr/lib/libpam.2.dylib（见 crates/strixmaid-helper/build.rs）。
# 这里对应 Linux 侧「DT_NEEDED 里有没有 libpam.so.0」那条断言。
otool -L "$bindir/strixmaid-helper" | grep -q 'libpam' || {
    echo "strixmaid-helper 没有链接 libpam，认证链会整条不可用" >&2
    otool -L "$bindir/strixmaid-helper" >&2
    exit 4
}

# --------------------------------------------------------------------------
# 组装
# --------------------------------------------------------------------------

# 带 -macos 后缀：`scripts/package.sh aarch64`（Linux 的 arm64 包）产出的是
# `strixmaid-<版本>-aarch64.tar.gz`，同名。今天 CI 的 Linux 侧只出 x86_64 所以
# 撞不上，但补上 Linux arm64 的那天两个包会在发布页互相覆盖。Windows 侧的
# `-x86_64-windows.zip` 本来就带平台名，这里跟上同一个惯例。
out="strixmaid-$version-$arch-macos"
stage=$(mktemp -d)
mkdir -p "$stage/$out/packaging/pam.d"

cp "$bindir/strixmaid"        "$stage/$out/"
cp "$bindir/strixmaid-helper" "$stage/$out/"
cp LICENSE                    "$stage/$out/"
cp packaging/macos/README.md  "$stage/$out/"
cp packaging/macos/install.sh packaging/macos/uninstall.sh "$stage/$out/packaging/"
cp packaging/macos/io.strixmaid.server.plist \
   packaging/macos/io.strixmaid.agent.plist  "$stage/$out/packaging/"
cp packaging/pam.d/strixmaid.macos           "$stage/$out/packaging/pam.d/"
chmod 0755 "$stage/$out/packaging/install.sh" "$stage/$out/packaging/uninstall.sh"

# COPYFILE_DISABLE=1：Apple 的 tar 默认会把扩展属性与资源分叉另存成一份
# `._文件名` 的 AppleDouble 条目塞进归档，别的系统上解出来就是一堆垃圾文件
# （而且其中可能夹带 com.apple.quarantine）。关掉它，包里就只有真正的文件。
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$out.tar.gz" "$out"
rm -rf "$stage"

ls -l "$out.tar.gz"
echo "体积（验收 §5.2：strixmaid ≤ 15MiB、agent ≤ 8MiB、helper ≤ 1MiB）："
ls -l "$bindir/strixmaid" "$bindir/strixmaid-agent" "$bindir/strixmaid-helper" \
    | awk '{print $5, $NF}'
