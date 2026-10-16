# Maintainer: Petar Margetić

pkgname=apc
pkgver=0.1.0
pkgrel=2
pkgdesc='Dual-pane Norton Commander inspired TUI package manager for Arch Linux'
arch=('x86_64')
url=''
license=('GPL-3.0-or-later')
depends=('pacman' 'glibc' 'gcc-libs')
optdepends=()
makedepends=('cargo')

# Adjust this path if your folder is still in ~/Projects/Gemini-test/apc
_projdir="${HOME}/Projects/${pkgname}"

prepare() {
    cd "$_projdir"
    # Ensure Cargo.lock is generated and up to date for --locked builds
    cargo generate-lockfile
}

build() {
    cd "$_projdir"

    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target

    LTOFLAGS= CFLAGS="${CFLAGS//-flto=auto/}" LDFLAGS="${LDFLAGS//-flto=auto/}" \
        cargo build --release --locked
}

check() {
    cd "$_projdir"

    export RUSTUP_TOOLCHAIN=stable
    cargo test --release --locked
}

package() {
    cd "$_projdir"

    # Install the compiled binary
    install -Dm755 "target/release/${pkgname}" \
        "${pkgdir}/usr/bin/${pkgname}"

    # Install documentation / license if present
    if [ -f "LICENSE" ]; then
        install -Dm644 LICENSE \
            "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE"
    fi

    if [ -f "README.md" ]; then
        install -Dm644 README.md \
            "${pkgdir}/usr/share/doc/${pkgname}/README.md"
    fi
}
