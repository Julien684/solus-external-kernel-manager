#!/usr/bin/env python3
"""
build_eopkg.py — Builds a .eopkg for solus-external-kernel-manager in the
exact Solus binary format.

The output is a ZIP containing:
  - metadata.xml
  - files.xml
  - install.tar.xz

No COMAR scripts (pure file package, no install/uninstall hooks needed).
No <RuntimeDependencies>: `eopkg install <url>` downloads a single file and
cannot resolve deps against the Solus repo database. Prerequisites
(qt6-base — system-provided —, dracut, systemd-boot) are documented in
README.md and the app prompts for the root password at runtime.
"""
import hashlib
import io
import os
import sys
import tarfile
import zipfile
from datetime import date
from pathlib import Path

# ──────────────────────────────────────────────────────────────────────────────
# Configuration
# ──────────────────────────────────────────────────────────────────────────────
SRC_DIR = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
OUT_DIR = Path(sys.argv[2]) if len(sys.argv) > 2 else SRC_DIR

PACKAGE_NAME   = "solus-external-kernel-manager"
VERSION        = "0.1.1"
RELEASE        = 2
DIST_RELEASE   = 1
ARCH           = "x86_64"
DIST_NAME      = "Solus"
BUILD_HOST     = "solus-local"
HOME_PAGE      = "https://depoa.julien68.fr/Linuxon/solus-external-kernel-manager"
PACKAGER_NAME  = "Julien"
PACKAGER_EMAIL = "julien_solus@use.startmail.com"
PART_OF        = "system.utilities"

# (dest_path, src_rel_path, mode)
# On Solus, /bin is a symlink to /usr/bin, so install to usr/bin.
FILE_LIST = [
    ("usr/bin/solus-external-kernel-manager",
     "target/release/solus-external-kernel-manager", 0o755),
    ("usr/share/applications/solus-external-kernel-manager.desktop",
     "solus-external-kernel-manager.desktop", 0o644),
    ("usr/share/doc/solus-external-kernel-manager/README.md",
     "README.md", 0o644),
    ("usr/share/licenses/solus-external-kernel-manager/LICENSE",
     "LICENSE", 0o644),
]

# Runtime dependencies.
#
# NOTE: `eopkg install <single-file .eopkg from URL>` does NOT resolve these
# against the Solus repo database — they are informational metadata only.
# The real enforcement (clear, actionable error at runtime) lives in
# `install_kernel` in src/install.rs, which checks for `dracut` before use.
# dracut is required to generate the initrd during kernel install.
RUNTIME_DEPS = ["dracut"]

OUT_FILENAME = f"{PACKAGE_NAME}-{VERSION}-{RELEASE}-{DIST_RELEASE}-{ARCH}.eopkg"

# French metadata — accents preserved in UTF-8 (must NOT pass through the
# ASCII-replacing helper `a()`).
_FR_SUMMARY = ("Gestionnaire graphique (Qt6) des noyaux externes "
               "(CachyOS, Zen, Liquorix) pour Solus - experimental")
_FR_DESCRIPTION = """solus-external-kernel-manager est un petit gestionnaire graphique
(Qt6) qui installe des noyaux externes sur une distribution Solus. Trois
fournisseurs sont pris en charge, chacun dans son onglet : CachyOS (depot
CachyOS, generation x86_64_v4 avec repli sur l'ancienne generation),
Zen (depot Arch extra) et Liquorix (liquorix.net). L'application lit la base
du depot, liste les noyaux disponibles, telecharge le paquet .pkg.tar.zst
avec verification SHA256, puis installe modules, vmlinuz, initrd (dracut)
et l'entree de boot systemd-boot/clr-boot-manager. La structure interne du
paquet est decouverte dynamiquement. Le noyau en cours est protege a la
suppression. Mode CLI fourni (--install, --fetch-test). Projet
experimental, developpe de facon 'vibe-coded'. Non officiel ; non affine a
Solus ni aux projets CachyOS, Zen ou Liquorix."""


# ──────────────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────────────
def sha1_hex(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def file_type_for(path: str) -> str:
    """Return the eopkg file type for a given path (no leading /)."""
    if path.startswith("usr/bin/") or path.startswith("bin/"):
        return "executable"
    if path.startswith("usr/libexec/") or path.startswith("usr/sbin/") or path.startswith("sbin/"):
        return "executable"
    if path.startswith("usr/include/"):
        return "header"
    if path.startswith("usr/lib/"):
        return "library"
    if path.startswith("usr/share/man/"):
        return "man"
    if path.startswith("usr/share/info/"):
        return "info"
    if path.startswith("usr/share/locale/"):
        return "localedata"
    if path.startswith("usr/share/doc/") or path.startswith("usr/share/gtk-doc/") or path.startswith("usr/share/help/"):
        return "doc"
    if path.startswith("usr/lib/pkgconfig/") or path.startswith("usr/lib32/pkgconfig/") or path.startswith("usr/lib64/pkgconfig/"):
        return "data"
    if path.startswith("etc/"):
        return "config"
    return "data"


# ──────────────────────────────────────────────────────────────────────────────
# Build
# ──────────────────────────────────────────────────────────────────────────────
def build():
    out_path = OUT_DIR / OUT_FILENAME
    print(f"Building: {out_path}")

    file_entries = []  # (dest_path, bytes, mode)
    for dest_path, src_rel, mode in FILE_LIST:
        src = SRC_DIR / src_rel
        if not src.exists():
            print(f"  WARNING: source file not found: {src}", file=sys.stderr)
            continue
        data = src.read_bytes()
        file_entries.append((dest_path, data, mode))
        print(f"  + {dest_path}  ({len(data)} bytes, mode {oct(mode)})")

    tar_buf = io.BytesIO()
    with tarfile.open(fileobj=tar_buf, mode="w:xz") as tar:
        for dest_path, data, mode in file_entries:
            info = tarfile.TarInfo(name=dest_path)
            info.size = len(data)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.mtime = 0
            tar.addfile(info, io.BytesIO(data))
    install_tar_xz = tar_buf.getvalue()
    print(f"  install.tar.xz: {len(install_tar_xz)} bytes")

    files_parts = ['<Files>']
    for dest_path, data, mode in file_entries:
        ftype = file_type_for(dest_path)
        mode_str = f"{mode:04o}"
        h = sha1_hex(data)
        files_parts.append("  <File>")
        files_parts.append(f"    <Path>{dest_path}</Path>")
        files_parts.append(f"    <Type>{ftype}</Type>")
        files_parts.append(f"    <Size>{len(data)}</Size>")
        files_parts.append(f"    <Uid>0</Uid>")
        files_parts.append(f"    <Gid>0</Gid>")
        files_parts.append(f"    <Mode>{mode_str}</Mode>")
        files_parts.append(f"    <Hash>{h}</Hash>")
        files_parts.append("  </File>")
    files_parts.append("</Files>")
    files_xml = "\n".join(files_parts).encode("utf-8")
    print(f"  files.xml: {len(files_xml)} bytes")

    today = date.today().isoformat()
    installed_size = sum(len(data) for _, data, _ in file_entries)

    deps_xml = ""
    if RUNTIME_DEPS:
        deps_xml = "    <RuntimeDependencies>\n"
        for dep in RUNTIME_DEPS:
            deps_xml += f"        <Dependency>{dep}</Dependency>\n"
        deps_xml += "    </RuntimeDependencies>\n"

    def a(s: str) -> str:
        return s.encode("ascii", "replace").decode("ascii")

    metadata_xml = f"""<PISI>
    <Source>
        <Name>{PACKAGE_NAME}</Name>
        <Homepage>{HOME_PAGE}</Homepage>
        <Packager>
            <Name>{PACKAGER_NAME}</Name>
            <Email>{PACKAGER_EMAIL}</Email>
        </Packager>
    </Source>
    <Package>
        <Name>{PACKAGE_NAME}</Name>
        <Summary xml:lang="en">{a("Graphical Qt6 manager to install external kernels (CachyOS, Zen, Liquorix) on Solus - experimental")}</Summary>
        <Description xml:lang="en">{a("""solus-external-kernel-manager is a small graphical Qt6 manager that
installs external kernels on a Solus distribution. Three providers are
supported, each in its own tab: CachyOS (CachyOS repository, x86_64_v4
generation with fallback to the previous one), Zen (Arch extra repository)
and Liquorix (liquorix.net). The application reads the repository database,
lists the available kernels, downloads the .pkg.tar.zst package with SHA256
verification, then installs the modules, vmlinuz, initrd (dracut) and the
systemd-boot/clr-boot-manager boot entry. The internal package layout is
discovered dynamically. The running kernel is protected against removal.
A CLI mode is provided (--install, --fetch-test). Experimental project,
vibe-coded. Unofficial; not affiliated with Solus, CachyOS, the Zen kernel
or Liquorix.""")}</Description>
        <Summary xml:lang="fr">{_FR_SUMMARY}</Summary>
        <Description xml:lang="fr">{_FR_DESCRIPTION}</Description>
        <PartOf>{PART_OF}</PartOf>
        <License>GPL-3.0-or-later</License>
{deps_xml}        <History>
            <Update release="2">
                <Date>{today}</Date>
                <Version>{VERSION}</Version>
                <Comment>{PACKAGE_NAME} {VERSION} rel2 : option CLI --list</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
            <Update release="1">
                <Date>2026-10-26</Date>
                <Version>{VERSION}</Version>
                <Comment>{PACKAGE_NAME} {VERSION} : fix Zen listing/removal (registry + modern module names), per-tab actions</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
        </History>
        <BuildHost>{BUILD_HOST}</BuildHost>
        <Distribution>{DIST_NAME}</Distribution>
        <DistributionRelease>{DIST_RELEASE}</DistributionRelease>
        <Architecture>{ARCH}</Architecture>
        <InstalledSize>{installed_size}</InstalledSize>
        <PackageFormat>1.2</PackageFormat>
        <Source>
            <Name>{PACKAGE_NAME}</Name>
            <Homepage>{HOME_PAGE}</Homepage>
            <Packager>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Packager>
        </Source>
    </Package>
</PISI>""".encode("utf-8")
    print(f"  metadata.xml: {len(metadata_xml)} bytes")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr("metadata.xml", metadata_xml)
        zf.writestr("files.xml", files_xml)
        zf.writestr("install.tar.xz", install_tar_xz)

    size = out_path.stat().st_size
    print(f"\n  ✓ {out_path}  ({size} bytes)")
    print(f"  SHA1: {sha1_hex(out_path.read_bytes())}")
    return str(out_path)


if __name__ == "__main__":
    build()
