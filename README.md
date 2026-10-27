# Solus External Kernel Manager

> **⚠ Projet expérimental** — développé de façon « vibe-coded ». Non officiel,
> non affilié aux projets **Solus**, **CachyOS**, **Zen kernel** ni
> **Liquorix**.

---

## 🇫🇷 Français

**Solus External Kernel Manager** est un petit gestionnaire graphique (Qt6,
écrit en Rust) qui installe et gère des **noyaux Linux externes** sur une
distribution **Solus**. Trois fournisseurs sont pris en charge, un par onglet
:

| Onglet | Fournisseur | Dépôt source |
|---|---|---|
| **CachyOS** | Noyaux CachyOS | `mirror.cachyos.org` (génération `x86_64_v4`, repli sur l'ancienne) |
| **Zen** | noyau `linux-zen` | dépôt `extra` d'Arch Linux |
| **Liquorix** | noyau `linux-lqx` | `liquorix.net` |

L'interface présente **trois onglets en haut** (un par fournisseur), chacun
portant ses propres boutons **Actualiser / Installer / Supprimer** ; la zone
commune en bas rassemble la liste des noyaux installés, le noyau de
démarrage (systemd-boot) et le journal d'activité.

### Ce que fait l'application

1. **Lecture du dépôt** : télécharge la base du dépôt (`.db`, formats zstd /
   gzip / xz supportés) et liste les noyaux disponibles avec leur **SHA256**
   et leur taille.
2. **Téléchargement vérifié** : récupère le paquet `.pkg.tar.zst` puis
   **vérifie le SHA256** avant toute installation.
3. **Découverte dynamique de la structure** : au lieu de supposer où se
   trouvent les fichiers dans le paquet, l'application **découvre** le dossier
   `usr/lib/modules/<…>` et le `vmlinuz` réels depuis l'archive. C'est le nom
   du dossier modules qui fait foi de l'identité du noyau (`uname -r`).
4. **Installation** (nécessite le mot de passe root) :
   - extraction des modules dans `/lib/modules/<nom>`, `depmod -a` ;
   - copie du `vmlinuz` sur l'ESP ( `/boot/EFI/<vendor>/kernel-<nom>` ) ;
   - génération de l'**initrd** avec `dracut` ;
   - création de l'**entrée de boot** systemd-boot / clr-boot-manager ;
   - le **noyau en cours d'utilisation est protégé** contre la suppression.
5. **Démarrage** : changement du noyau par défaut via
   `bootctl set-default` / l'entrée systemd-boot correspondante.

### Installation du paquet

Prérequis sur la machine cible : **Qt6 Base** (fourni par le système Solus),
**dracut**, **systemd-boot** (ou `clr-boot-manager`) et un **ESP monté en
`/boot`**.

```bash
# Installer depuis le dépôt eopkg « linuxon » (recommandé)
sudo eopkg install -y solus-external-kernel-manager

# … ou directement depuis le dépôt GitHub
sudo eopkg install -y https://raw.githubusercontent.com/Julien684/solus-external-kernel-manager/main/solus-external-kernel-manager-0.1.1-2-1-x86_64.eopkg
```

Le paquet est publié dans le dépôt eopkg personnel `linuxon`
(GitHub `Julien684/eopkg-repo` ; déjà configuré sur vos machines).

Puis lancer « Solus External Kernel Manager » depuis le menu d'applications.

### Utilisation de l'interface

1. Choisir l'onglet du fournisseur souhaité (CachyOS / Zen / Liquorix).
2. Cliquer **Rafraîchir** pour interroger le dépôt et lister les noyaux.
3. Cliquer **Installer** sur la version voulue : le SHA256 est vérifié,
   l'application demande le **mot de passe root** et exécute l'installation.
4. Le noyau apparaît alors dans la liste « Noyaux installés » (zone commune).
5. Pour démarrer dessus : sélectionner le noyau et cliquer
   **Démarre** (modifie l'entrée systemd-boot par défaut), puis redémarrer.
6. La **suppression** d'un noyau externe retire modules, fichiers ESP et
   entrée de boot. Le noyau en cours (`uname -r`) ne peut pas être supprimé.

### Mode ligne de commande (sans GUI)

Handy pour les tests ou l'automatisation :

```bash
# Tester la lecture des trois dépôts
solus-external-kernel-manager --fetch-test

# … et en plus télécharger un paquet et vérifier son SHA256
solus-external-kernel-manager --fetch-test --download

# Installer un noyau sans GUI (mot de passe root via variable)
SEKM_SUDO_PASS=*** solus-external-kernel-manager --install liquorix
#    <kind> ∈ { cachyos | zen | liquorix }

# Lister les noyaux externes installés
solus-external-kernel-manager --list
```

### Notes techniques / sécurité

- **Secure Boot** : les noyaux tiers (`vmlinuz` non signé) **ne démarrent pas
  si Secure Boot est activé**. Sur une machine physique, il faut soit le
  désactiver, soit signer les noyaux (`mokutil`). Sur une machine virtuelle
  libvirt/QEMU, utiliser un template NVRAM OVMF **sans** Secure Boot
  (`OVMF_VARS_4M.fd`, l'image sans suffixe `.ms`).
- L'application ne modifie jamais les noyaux natifs Solus ; elle n'installe
  que des noyaux « externes » dans leur arborescence dédiée.
- Journal des opérations : `~/.local/state/solus-external-kernel-manager/manager.log`.

### Construction depuis les sources

```bash
# Prérequis : Rust (≥ 1.85) + les libs de développement Qt6 (qmake6, Qt6 Widgets)
cargo build --release
# Binaire : target/release/solus-external-kernel-manager

# Tests unitaires
cargo test --release

# Générer le paquet .eopkg
python3 build_eopkg.py . .
```

---

## 🇬🇧 English

**Solus External Kernel Manager** is a small graphical manager (Qt6, written
in Rust) that installs and manages **external Linux kernels** on a **Solus**
distribution. Three providers are supported, one per tab:

| Tab | Provider | Source repository |
|---|---|---|
| **CachyOS** | CachyOS kernels | `mirror.cachyos.org` (`x86_64_v4` generation, fallback to the previous one) |
| **Zen** | `linux-zen` kernel | Arch Linux `extra` repository |
| **Liquorix** | `linux-lqx` kernel | `liquorix.net` |

The UI shows **three tabs on top** (one per provider), each with its own
**Refresh / Install / Remove** buttons; the shared area below holds the
installed-kernels list, the boot kernel (systemd-boot) and an activity log.

### What the application does

1. **Reads the repository**: downloads the repository database (`.db`,
   zstd / gzip / xz formats supported) and lists the available kernels with
   their **SHA256** and size.
2. **Verified download**: fetches the `.pkg.tar.zst` package and **checks the
   SHA256** before any installation.
3. **Dynamic layout discovery**: instead of assuming where files live inside
   the package, the application **discovers** the real `usr/lib/modules/<…>`
   directory and `vmlinuz` from the archive. The modules-directory name is the
   kernel's canonical identity (`uname -r`).
4. **Install** (requires the root password):
   - extracts the modules into `/lib/modules/<name>`, runs `depmod -a`;
   - copies the `vmlinuz` to the ESP (`/boot/EFI/<vendor>/kernel-<name>`);
   - generates the **initrd** with `dracut`;
   - creates the **boot entry** for systemd-boot / clr-boot-manager;
   - the **currently running kernel is protected** against removal.
5. **Boot**: changes the default kernel via `bootctl set-default` / the
   matching systemd-boot entry.

### Installing the package

Prerequisites on the target machine: **Qt6 Base** (provided by Solus),
**dracut**, **systemd-boot** (or `clr-boot-manager`) and an **ESP mounted at
`/boot`**.

```bash
# Install from the personal eopkg repo "linuxon" (recommended)
sudo eopkg install -y solus-external-kernel-manager

# … or directly from the GitHub repository
sudo eopkg install -y https://raw.githubusercontent.com/Julien684/solus-external-kernel-manager/main/solus-external-kernel-manager-0.1.1-2-1-x86_64.eopkg
```

The package is published in the personal `linuxon` eopkg repository
(GitHub `Julien684/eopkg-repo`; already configured on your machines).

Then launch "Solus External Kernel Manager" from the application menu.

### Using the interface

1. Pick the provider tab you want (CachyOS / Zen / Liquorix).
2. Click **Refresh** to query the repository and list the kernels.
3. Click **Install** on the desired version: the SHA256 is verified, the
   application asks for the **root password** and runs the install.
4. The kernel then shows up in the "Installed kernels" list (shared area).
5. To boot it: select the kernel and click **Boot** (changes the systemd-boot
   default entry), then reboot.
6. **Removing** an external kernel deletes its modules, ESP files and boot
   entry. The running kernel (`uname -r`) cannot be removed.

### Command-line mode (no GUI)

Handy for testing or automation:

```bash
# Test reading the three repositories
solus-external-kernel-manager --fetch-test

# … plus download a package and verify its SHA256
solus-external-kernel-manager --fetch-test --download

# Install a kernel without the GUI (root password via environment)
SEKM_SUDO_PASS=*** solus-external-kernel-manager --install liquorix
#    <kind> ∈ { cachyos | zen | liquorix }

# List installed external kernels
solus-external-kernel-manager --list
```

### Technical / security notes

- **Secure Boot**: third-party kernels (unsigned `vmlinuz`) **will not boot
  if Secure Boot is enabled**. On physical hardware you must either disable
  it or sign the kernels (`mokutil`). On a libvirt/QEMU virtual machine, use
  an OVMF NVRAM template **without** Secure Boot (`OVMF_VARS_4M.fd`, the image
  without the `.ms` suffix).
- The application never touches native Solus kernels; it only installs
  "external" kernels into their dedicated tree.
- Operation log: `~/.local/state/solus-external-kernel-manager/manager.log`.

### Building from source

```bash
# Prerequisites: Rust (≥ 1.85) + Qt6 development libraries (qmake6, Qt6 Widgets)
cargo build --release
# Binary: target/release/solus-external-kernel-manager

# Unit tests
cargo test --release

# Generate the .eopkg package
python3 build_eopkg.py . .
```
