//! Récupération d'un noyau depuis **kernel.org** : liste des versions
//! publiées, téléchargement de l'archive `linux-<ver>.tar.xz`, vérification de
//! la signature PGP détachée (`.sign`) et empreinte de la clé signataire.
//!
//! La vérification PGP n'est pas optionnelle : l'archive est compilée puis
//! installée avec les droits root. Trois cas, dans l'ordre :
//!   1. la clé signataire est déjà dans le trousseau de l'utilisateur →
//!      `gpg --verify` doit répondre `GOODSIG` + `VALIDSIG` ;
//!   2. elle ne l'est pas → on l'importe depuis `keys.openpgp.org` (le
//!      serveur de clés qui publie des identités vérifiées) puis on revérifie ;
//!   3. toujours pas de signature valide → échec, rien n'est compilé.
//!
//! L'empreinte de la clé qui a signé est ensuite comparée à un **magasin
//! TOFU** (`~/.local/state/solus-external-kernel-manager/signing-keys.json`) :
//! une nouvelle clé est enregistrée et signalée dans le journal, une clé
//! connue doit correspondre exactement. Un changement de clé pour un noyau
//! kernel.org n'a aucune raison légitime : c'est une alerte, pas un détail.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ureq::Agent;

/// Source de la liste des versions publiées (JSON officiel de kernel.org).
pub const RELEASES_URL: &str = "https://www.kernel.org/releases.json";

/// Serveurs de clés essayés pour importer une clé signataire absente.
///
/// `keys.openpgp.org` sert les clés **sans identité** (UID retirés tant que le
/// propriétaire ne les a pas vérifiées) : l'import y « réussit » en code de
/// sortie non nul et gpg affiche `pas d'identité`, mais la matière de la clé
/// est bien présente. On revérifie donc la signature après chaque tentative, et
/// on bascule sur `keyserver.ubuntu.com` (clés complètes) si besoin.
pub const KEYSERVERS: &[&str] = &["hkps://keys.openpgp.org", "hkps://keyserver.ubuntu.com"];

/// Une version de noyau publiée sur kernel.org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgRelease {
    /// Version nue, ex. `6.12.118` (ou `7.4-rc3`).
    pub version: String,
    /// Canal : `mainline`, `stable` ou `longterm`.
    pub moniker: String,
    /// Fin de maintenance annoncée par kernel.org.
    pub eol: bool,
    /// Pré-version de développement (`-rc`) : pas de signature PGP publiée.
    pub rc: bool,
    /// URL de l'archive `tar.xz` (champ `source` du JSON).
    pub tarball_url: String,
}

impl OrgRelease {
    /// Libellé pour la liste déroulante : `6.12.118 — longterm (LTS)`.
    pub fn label(&self) -> String {
        let mut s = format!("{} — {}", self.version, self.moniker);
        if self.rc {
            s.push_str(" (RC, non signé)");
            return s;
        }
        if self.eol {
            s.push_str(" (EOL)");
        } else if self.moniker == "longterm" {
            s.push_str(" (LTS)");
        }
        s
    }

    /// Nom de fichier de l'archive : le **basename de l'URL** publiée.
    ///
    /// On ne peut pas le composer (`linux-<ver>.tar.xz`) : pour un RC,
    /// kernel.org publie `https://git.kernel.org/torvalds/t/linux-7.4-rc3.tar.gz`
    /// — une autre extension, un autre hôte.
    pub fn tarball_name(&self) -> String {
        self.tarball_url
            .rsplit('/')
            .next()
            .filter(|s| s.contains(".tar."))
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("linux-{}.tar.xz", self.version))
    }

    /// URL de la signature détachée.
    ///
    /// Attention à la convention kernel.org, qui n'est pas intuitive : la
    /// signature s'appelle `linux-<version>.tar.sign` (sur le **tar
    /// décompressé**, pas sur le `.tar.xz`) et non `.tar.xz.sign`. Demander
    /// `.tar.xz.sign` renvoie un 404.
    pub fn signature_url(&self) -> String {
        format!("{}.sign", strip_compression_suffix(&self.tarball_url))
    }

    /// Nom local de la signature (`linux-<version>.tar.sign`).
    pub fn signature_name(&self) -> String {
        format!("{}.sign", strip_compression_suffix(&self.tarball_name()))
    }

    /// Vrai si la version ne contient que des caractères sûrs (elle sert de
    /// nom de fichier et de composant de chemin).
    pub fn is_safe(&self) -> bool {
        version_is_safe(&self.version)
    }
}

/// Retire un suffixe de compression d'un nom de fichier ou d'une URL.
/// `linux-6.12.118.tar.xz` → `linux-6.12.118.tar`.
fn strip_compression_suffix(s: &str) -> &str {
    for ext in [".xz", ".gz", ".bz2"] {
        if let Some(base) = s.strip_suffix(ext) {
            return base;
        }
    }
    s
}

/// Échappe une chaîne pour un usage entre guillemets simples dans `sh -c`.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Une version n'est acceptée que si elle est composée de `[0-9A-Za-z._-]` et
/// non vide : elle finit en nom de fichier et en argument de ligne de commande.
pub fn version_is_safe(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && v.chars().any(|c| c.is_ascii_digit())
}

/// Analyse le JSON de `releases.json`. Pure (testable hors réseau).
///
/// On ne garde que les canaux qui correspondent à des noyaux utilisables
/// (`mainline`, `stable`, `longterm`) ; `linux-next` est un instantané de
/// développement et n'a pas sa place dans un gestionnaire de noyaux.
pub fn parse_releases(json: &str) -> Result<Vec<OrgRelease>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("JSON kernel.org illisible : {}", e))?;
    let arr = v
        .get("releases")
        .and_then(|r| r.as_array())
        .ok_or("le JSON de kernel.org ne contient pas de tableau `releases`")?;
    let mut out: Vec<OrgRelease> = Vec::new();
    let mut out_rc: usize = 0;
    for r in arr {
        let moniker = r.get("moniker").and_then(|x| x.as_str()).unwrap_or("");
        if !matches!(moniker, "mainline" | "stable" | "longterm") {
            continue;
        }
        let version = r.get("version").and_then(|x| x.as_str()).unwrap_or("");
        if !version_is_safe(version) {
            continue;
        }
        let eol = r.get("iseol").and_then(|x| x.as_bool()).unwrap_or(false);
        // `source` est l'URL officielle de l'archive. Si elle manque (vieux
        // format de JSON), on la reconstruit d'après la version majeure.
        // `linux-next` est un instantané de développement, pas un noyau
        // utilisable : écarté. Les RC restent proposés mais sont marqués
        // « non signé » : kernel.org ne publie pas de signature détachée pour
        // eux (uniquement l'arbre git), donc l'utilisateur devra confirmer.
        if version.contains("-next") {
            continue;
        }
        let tarball_url = r
            .get("source")
            .and_then(|x| x.as_str())
            .filter(|s| {
                s.ends_with(".tar.xz") || s.ends_with(".tar.gz") || s.ends_with(".tar.bz2")
            })
            .map(|s| s.to_string())
            .unwrap_or_else(|| derive_tarball_url(version));
        let rc = version.contains("-rc");
        if rc {
            out_rc += 1;
        }
        out.push(OrgRelease {
            version: version.to_string(),
            moniker: moniker.to_string(),
            eol,
            rc,
            tarball_url,
        });
    }
    if out_rc > 0 {
        crate::sysutil::log(&format!(
            "{} version(s) -rc listée(s) : aucune signature PGP n'est publiée pour ces \
             instantanés, une confirmation explicite sera demandée avant compilation.",
            out_rc
        ));
    }
    // Les versions en fin de vie passent en dernier, sinon on garde l'ordre
    // de kernel.org (mainline, stable, longterm du plus récent au plus ancien).
    out.sort_by_key(|r| r.eol);
    Ok(out)
}

/// URL de repli si le JSON ne fournit pas `source` : kernel.org range les
/// archives par version majeure (`v6.x/linux-6.12.118.tar.xz`).
fn derive_tarball_url(version: &str) -> String {
    let major = version.split('.').next().unwrap_or("");
    format!(
        "https://cdn.kernel.org/pub/linux/kernel/v{}.x/linux-{}.tar.xz",
        major, version
    )
}

fn agent() -> Agent {
    ureq::AgentBuilder::new()
        .user_agent("solus-external-kernel-manager/0.2 (Solus)")
        .timeout(Duration::from_secs(120))
        .build()
}

/// Récupère et analyse la liste des versions publiées sur kernel.org.
pub fn fetch_releases() -> Result<Vec<OrgRelease>, String> {
    let resp = agent()
        .get(RELEASES_URL)
        .call()
        .map_err(|e| format!("kernel.org injoignable : {}", e))?;
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("lecture de releases.json impossible : {}", e))?;
    parse_releases(&body)
}

/// Version correspondant exactement à `version`, ou une version construite à
/// la main (l'utilisateur peut vouloir compiler une version absente du JSON).
pub fn resolve_release(known: &[OrgRelease], version: &str) -> Result<OrgRelease, String> {
    if !version_is_safe(version) {
        return Err(format!(
            "version refusée (caractères autorisés : chiffres, lettres, point, tiret) : « {} »",
            version
        ));
    }
    if let Some(r) = known.iter().find(|r| r.version == version) {
        return Ok(r.clone());
    }
    Ok(OrgRelease {
        version: version.to_string(),
        moniker: "manuel".to_string(),
        eol: false,
        rc: version.contains("-rc"),
        tarball_url: derive_tarball_url(version),
    })
}

/// Télécharge l'archive d'une version (et sa signature détachée).
/// Retourne `(archive, signature)`.
pub fn download(
    rel: &OrgRelease,
    dir: &Path,
    on_progress: &dyn Fn(u64, u64),
    require_sig: bool,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    if !rel.is_safe() {
        return Err(format!("version refusée : « {} »", rel.version));
    }
    let _ = std::fs::create_dir_all(dir);
    let tar = dir.join(rel.tarball_name());
    let sig = dir.join(rel.signature_name());

    if !(tar.exists() && tar.metadata().map(|m| m.len() > 0).unwrap_or(false)) {
        crate::sysutil::log(&format!("Téléchargement de {} …", rel.tarball_url));
        fetch_to_file(&rel.tarball_url, &tar, on_progress)?;
    } else {
        crate::sysutil::log(&format!(
            "Archive déjà présente : {} (téléchargement évité)",
            tar.display()
        ));
        on_progress(
            tar.metadata().map(|m| m.len()).unwrap_or(0),
            tar.metadata().map(|m| m.len()).unwrap_or(0),
        );
    }

    // La signature est toujours re-téléchargée : elle est minuscule (~800 o) et
    // une signature périmée est le scénario qu'on veut exclure.
    if let Err(e) = fetch_to_file(&rel.signature_url(), &sig, &|_, _| {}) {
        if require_sig {
            return Err(format!(
                "signature PGP introuvable ({}). Kernel.org publie `linux-<version>.tar.sign` \
                 (signature du tar décompressé) à côté de chaque archive ; sans elle, la \
                 compilation est refusée.",
                e
            ));
        }
        crate::sysutil::log(&format!(
            "AUCUNE SIGNATURE pour {} ({}) — compilation non vérifiable, acceptée par \
             confirmation explicite de l'utilisateur.",
            rel.version, e
        ));
        return Ok((tar, None));
    }
    // Un `.sign` vide ou tronqué (miroir cassé, réponse coupée) passerait le
    // téléchargement puis échouerait dans gpg avec un message obscur : on le
    // rejette ici, avec la taille réelle sous les yeux.
    let sig_len = sig.metadata().map(|m| m.len()).unwrap_or(0);
    let head = std::fs::read(&sig).unwrap_or_default();
    if sig_len < 100 || !head.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        let _ = std::fs::remove_file(&sig);
        return Err(format!(
            "la signature téléchargée pour {} n'est pas une signature PGP détachable ({} octets). \
             Compilation refusée.",
            rel.version, sig_len
        ));
    }
    Ok((tar, Some(sig)))
}

/// Télécharge `url` vers `dest` en flux, avec progression.
fn fetch_to_file(url: &str, dest: &Path, on_progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| format!("téléchargement de {} impossible : {}", url, e))?;
    let total: u64 = resp
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let tmp = dest.with_extension("part");
    let mut out = std::fs::File::create(&tmp)
        .map_err(|e| format!("création de {} impossible : {}", tmp.display(), e))?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut received: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("lecture de {} interrompue : {}", url, e))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n])
            .map_err(|e| format!("écriture de {} impossible : {}", tmp.display(), e))?;
        received += n as u64;
        on_progress(received, total);
    }
    drop(out);
    if total > 0 && received != total {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "téléchargement incomplet ({} / {} octets)",
            received, total
        ));
    }
    std::fs::rename(&tmp, dest)
        .map_err(|e| format!("renommage vers {} impossible : {}", dest.display(), e))?;
    Ok(())
}

/// Clé PGP qui a signé une archive, telle que rapportée par `gpg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningKey {
    /// Empreinte complète (hexadécimal majuscule) de la clé primaire.
    pub fingerprint: String,
    /// Identité déclarée (`uid`) de la clé.
    pub uid: String,
    /// Clé déjà connue du magasin TOFU avant cette vérification.
    pub known: bool,
}

/// Vérifie la signature détachée d'une archive. Importe la clé depuis
/// `keys.openpgp.org` si elle est absente du trousseau, puis exige une
/// signature valide. Retourne la clé signataire.
pub fn verify_signature(tarball: &Path, sig: &Path) -> Result<SigningKey, String> {
    if !has_bin("gpg") {
        return Err(
            "`gpg` est requis pour vérifier la signature de l'archive. Sur Solus : \
             `sudo eopkg install gnupg`, puis relancez."
                .to_string(),
        );
    }
    if !has_bin("xz") {
        return Err("`xz` est requis pour décompresser l'archive avant vérification \
                    de la signature (le `.sign` de kernel.org porte sur le tar décompressé)."
            .to_string());
    }
    let (code, out, err) = gpg_verify(tarball, sig)?;
    if code == 0 {
        return key_from_status(&out).ok_or_else(|| {
            "gpg a accepté la signature mais n'a pas rapporté d'empreinte exploitable".to_string()
        });
    }
    // `xz` en amont du tube : un échec de décompression n'est pas une mauvaise
    // signature, et le message doit le dire.
    if err.contains("xz:") {
        return Err(format!(
            "l'archive {} est illisible (xz) : {}",
            tarball.display(),
            tail(&err)
        ));
    }
    let missing = status_field(&out, "NO_PUBKEY")
        .or_else(|| status_field(&err, "NO_PUBKEY"))
        .or_else(|| {
            err.split_whitespace()
                .find(|w| w.len() == 16 && w.chars().all(|c| c.is_ascii_hexdigit()))
                .map(|s| s.to_string())
        });
    let keyid = match missing {
        Some(k) => k,
        None => {
            return Err(format!(
                "signature PGP INVALIDE pour {} : {}. L'archive est rejetée.",
                tarball.display(),
                tail(&err)
            ))
        }
    };

    // Clé absente : on tente chaque serveur puis on revérifie. Le code de
    // sortie de l'import n'est pas la preuve — la signature l'est.
    for ks in KEYSERVERS {
        crate::sysutil::log(&format!(
            "Clé PGP {} absente du trousseau — import depuis {} …",
            keyid, ks
        ));
        let (_ic, _io, ie) = run(
            "gpg",
            &["--batch", "--keyserver", ks, "--recv-key", &keyid],
        );
        if !ie.trim().is_empty() {
            crate::sysutil::log(&format!("  gpg import : {}", tail(&ie)));
        }
        let (c2, out2, err2) = gpg_verify(tarball, sig)?;
        if c2 == 0 {
            return key_from_status(&out2).ok_or_else(|| {
                "gpg a accepté la signature mais n'a pas rapporté d'empreinte exploitable"
                    .to_string()
            });
        }
        crate::sysutil::log(&format!(
            "  signature toujours invalide après import depuis {} : {}",
            ks,
            tail(&err2)
        ));
    }
    Err(format!(
        "la clé PGP {} qui a signé {} n'a pas pu être obtenue (essayé : {}). \
         Importez-la à la main, par exemple :\n  gpg --keyserver {} --recv-key {}\n\
         puis relancez.",
        keyid,
        tarball.file_name().unwrap_or_default().to_string_lossy(),
        KEYSERVERS.join(", "),
        KEYSERVERS[1],
        keyid
    ))
}

/// Vérifie la signature au format kernel.org.
///
/// La signature porte sur le **tar décompressé** : `gpg --verify sig.tar.xz`
/// échoue toujours. On décompresse donc en flux (`xz -dc`) et on passe le
/// résultat à gpg sur l'entrée standard (`-`), ce qui évite d'écrire 1,5 Go de
/// tar sur le disque.
fn gpg_verify(tarball: &Path, sig: &Path) -> Result<(i32, String, String), String> {
    let script = format!(
        "xz -dc {} | gpg --batch --no-tty --status-fd 1 --verify {} -",
        shq(&tarball.to_string_lossy()),
        shq(&sig.to_string_lossy())
    );
    Ok(run("sh", &["-c", &script]))
}

/// Extrait la valeur d'un champ `[GNUPG:] <NOM> <valeur>` du flux de statut.
fn status_field(status: &str, name: &str) -> Option<String> {
    for line in status.lines() {
        let rest = match line.strip_prefix("[GNUPG:] ") {
            Some(r) => r,
            None => continue,
        };
        let mut it = rest.split_whitespace();
        if it.next() == Some(name) {
            return it.next().map(|s| s.to_string());
        }
    }
    None
}

/// Construit la [`SigningKey`] depuis `GOODSIG` (identité) et `VALIDSIG`
/// (empreinte). `VALIDSIG` liste l'empreinte de la clé de signature puis,
/// éventuellement, celle de la clé primaire dont elle dépend : on retient la
/// primaire, c'est elle qui identifie durablement le signataire.
fn key_from_status(status: &str) -> Option<SigningKey> {
    let uid = status
        .lines()
        .filter_map(|l| l.strip_prefix("[GNUPG:] GOODSIG "))
        .map(|rest| {
            // `GOODSIG <keyid> <identité…>`
            rest.split_once(' ').map(|(_, u)| u.trim().to_string()).unwrap_or_default()
        })
        .find(|u| !u.is_empty())
        .unwrap_or_else(|| "(identité inconnue)".to_string());

    let validsig = status
        .lines()
        .find_map(|l| l.strip_prefix("[GNUPG:] VALIDSIG "))?;
    let tokens: Vec<&str> = validsig.split_whitespace().collect();
    let is_hex = |s: &str| s.len() >= 40 && s.chars().all(|c| c.is_ascii_hexdigit());
    let primary = tokens
        .iter()
        .rev()
        .find(|t| is_hex(t))
        .copied()
        .or_else(|| tokens.first().copied().filter(|t| is_hex(t)))?;
    Some(SigningKey {
        fingerprint: primary.to_ascii_uppercase(),
        uid,
        known: false,
    })
}

// ----------------------------------------------------------------------
// Magasin TOFU des clés signataires
// ----------------------------------------------------------------------

/// Entrée du magasin : identité, première vue, dernière version vérifiée.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyRecord {
    pub uid: String,
    pub first_seen: String,
    pub last_version: String,
}

/// Chemin du magasin de clés.
pub fn key_store_path() -> PathBuf {
    crate::sysutil::state_dir().join("signing-keys.json")
}

/// Lit le magasin (`{}` s'il est absent ou illisible).
pub fn read_key_store() -> HashMap<String, KeyRecord> {
    std::fs::read_to_string(key_store_path())
        .ok()
        .and_then(|c| serde_json::from_str::<HashMap<String, KeyRecord>>(&c).ok())
        .unwrap_or_default()
}

fn write_key_store(map: &HashMap<String, KeyRecord>) {
    let path = key_store_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(&tmp, json + "\n");
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Enregistre (ou met à jour) la clé signataire d'une version.
///
/// Retourne `Ok(true)` si la clé était déjà connue, `Ok(false)` s'il s'agit
/// d'une première rencontre (à signaler à l'utilisateur).
pub fn remember_key(key: &SigningKey, version: &str) -> Result<bool, String> {
    let mut store = read_key_store();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if let Some(rec) = store.get_mut(&key.fingerprint) {
        rec.uid = key.uid.clone();
        rec.last_version = version.to_string();
        write_key_store(&store);
        return Ok(true);
    }
    store.insert(
        key.fingerprint.clone(),
        KeyRecord {
            uid: key.uid.clone(),
            first_seen: today,
            last_version: version.to_string(),
        },
    );
    write_key_store(&store);
    Ok(false)
}

/// Résume lisible d'une vérification réussie, pour le journal.
pub fn describe_key(key: &SigningKey, first_time: bool) -> String {
    if first_time {
        format!(
            "Signature PGP valide — NOUVELLE clé enregistrée : {} ({})",
            key.uid, key.fingerprint
        )
    } else {
        format!(
            "Signature PGP valide — clé connue : {} ({})",
            key.uid, key.fingerprint
        )
    }
}

/// SHA256 d'un fichier (flux, pas de lecture intégrale en mémoire).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("ouverture de {} impossible : {}", path.display(), e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Lance une commande et retourne (code, stdout, stderr).
fn run(cmd: &str, args: &[&str]) -> (i32, String, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// Vrai si le binaire est dans le `PATH`.
pub fn has_bin(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", name))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tail(s: &str) -> String {
    let all: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = all.len().saturating_sub(4);
    all[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "latest_stable": {"version": "7.3.5", "moniker": "stable"},
      "releases": [
        {"moniker":"mainline","version":"7.4-rc3","source":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.4-rc3.tar.xz","iseol":false},
        {"moniker":"stable","version":"7.3.5","source":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.5.tar.xz","iseol":false},
        {"moniker":"stable","version":"7.2.15","source":"https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.15.tar.xz","iseol":true},
        {"moniker":"longterm","version":"6.12.118","source":"https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.118.tar.xz","iseol":false},
        {"moniker":"linux-next","version":"next-20261119","iseol":false}
      ]
    }"#;

    #[test]
    fn parse_keeps_rc_but_flags_it_and_drops_linux_next() {
        let r = parse_releases(SAMPLE).unwrap();
        // linux-next est écarté ; les RC restent proposés mais marqués, car
        // kernel.org ne publie aucune signature pour eux.
        assert_eq!(r.len(), 4, "releases retenues : {:?}", r);
        assert!(r.iter().all(|x| x.moniker != "linux-next"));
        let rc = r.iter().find(|x| x.version == "7.4-rc3").expect("RC listé");
        assert!(rc.rc, "le RC doit être marqué");
        assert!(rc.label().contains("non signé"), "label : {}", rc.label());
        assert!(r.iter().any(|x| x.version == "6.12.118"));
        assert!(r.iter().any(|x| x.version == "7.3.5"));
        assert!(!r.iter().find(|x| x.version == "7.3.5").unwrap().rc);
    }

    #[test]
    fn parse_accepts_a_gz_source_url() {
        // Cas réel : la mainline est publiée sur git.kernel.org en .tar.gz.
        // L'ancien filtre ne gardait que `.tar.xz` et reconstruisait une URL
        // cdn inexistante (404 au premier clic sur « Préparer »).
        let json = r#"{"releases":[{"moniker":"stable","version":"7.4",
            "source":"https://git.kernel.org/torvalds/t/linux-7.4.tar.gz"}]}"#;
        let r = parse_releases(json).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].tarball_url, "https://git.kernel.org/torvalds/t/linux-7.4.tar.gz");
        assert_eq!(r[0].tarball_name(), "linux-7.4.tar.gz");
        assert!(r[0].signature_url().ends_with("linux-7.4.tar.sign"));
    }

    #[test]
    fn parse_puts_eol_last() {
        let r = parse_releases(SAMPLE).unwrap();
        assert_eq!(r.last().unwrap().version, "7.2.15");
        assert!(r.last().unwrap().eol);
    }

    #[test]
    fn parse_uses_source_url_from_json() {
        let r = parse_releases(SAMPLE).unwrap();
        let lts = r.iter().find(|x| x.version == "6.12.118").unwrap();
        assert!(lts.tarball_url.ends_with("/v6.x/linux-6.12.118.tar.xz"));
        assert_eq!(lts.tarball_name(), "linux-6.12.118.tar.xz");
    }

    #[test]
    fn parse_derives_url_when_source_missing() {
        let json = r#"{"releases":[{"moniker":"stable","version":"6.6.162"}]}"#;
        let r = parse_releases(json).unwrap();
        assert_eq!(
            r[0].tarball_url,
            "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.6.162.tar.xz"
        );
    }

    #[test]
    fn parse_rejects_broken_json() {
        assert!(parse_releases("{not json").is_err());
        assert!(parse_releases("{}").is_err());
    }

    #[test]
    fn signature_is_on_the_uncompressed_tar() {
        // Convention kernel.org : `linux-X.Y.Z.tar.sign`, pas `.tar.xz.sign`.
        let rel = OrgRelease {
            version: "6.12.118".into(),
            moniker: "longterm".into(),
            eol: false,
            rc: false,
            tarball_url: "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.118.tar.xz"
                .into(),
        };
        assert_eq!(rel.signature_name(), "linux-6.12.118.tar.sign");
        assert_eq!(
            rel.signature_url(),
            "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.118.tar.sign"
        );
        // Idem pour une archive gzip (vieilles branches).
        let gz = OrgRelease {
            tarball_url: "https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-4.19.1.tar.gz"
                .into(),
            ..rel.clone()
        };
        assert!(gz.signature_url().ends_with("linux-4.19.1.tar.sign"));
    }

    #[test]
    fn shell_quoting_survives_quotes() {
        assert_eq!(shq("abc"), "'abc'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }

    #[test]
    fn version_safety() {
        assert!(version_is_safe("6.12.118"));
        assert!(version_is_safe("7.4-rc3"));
        assert!(!version_is_safe(""));
        assert!(!version_is_safe("../etc/passwd"));
        assert!(!version_is_safe("6.12.118;rm -rf /"));
        assert!(!version_is_safe("abc"));
    }

    #[test]
    fn resolve_known_version_keeps_metadata() {
        let known = parse_releases(SAMPLE).unwrap();
        let r = resolve_release(&known, "6.12.118").unwrap();
        assert_eq!(r.moniker, "longterm");
        assert!(r.tarball_url.contains("v6.x"));
    }

    #[test]
    fn resolve_unknown_version_is_built_from_version() {
        let known = parse_releases(SAMPLE).unwrap();
        let r = resolve_release(&known, "6.1.192").unwrap();
        assert_eq!(r.moniker, "manuel");
        assert_eq!(
            r.tarball_url,
            "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.1.192.tar.xz"
        );
    }

    #[test]
    fn resolve_rejects_unsafe_version() {
        let known = parse_releases(SAMPLE).unwrap();
        assert!(resolve_release(&known, "../../etc/passwd").is_err());
    }

    #[test]
    fn status_field_reads_no_pubkey() {
        let s = "[GNUPG:] NEWSIG\n[GNUPG:] ERRSIG ABAF11C65A2970B1 1 10 00 1700000000 9 -\n\
                 [GNUPG:] NO_PUBKEY ABAF11C65A2970B1\n";
        assert_eq!(
            status_field(s, "NO_PUBKEY").as_deref(),
            Some("ABAF11C65A2970B1")
        );
        assert_eq!(status_field(s, "VALIDSIG"), None);
    }

    #[test]
    fn key_from_status_prefers_primary_fingerprint() {
        // Signature faite par une sous-clé : l'empreinte primaire est le
        // dernier champ de VALIDSIG.
        let sub = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let prim = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let s = format!(
            "[GNUPG:] GOODSIG 1234567890ABCDEF Greg Kroah-Hartman <gregkh@linuxfoundation.org>\n\
             [GNUPG:] VALIDSIG {} 2026-11-01 1793000000 0 4 0 22 10 00 {}\n",
            sub, prim
        );
        let k = key_from_status(&s).unwrap();
        assert_eq!(k.fingerprint, prim);
        assert_eq!(k.uid, "Greg Kroah-Hartman <gregkh@linuxfoundation.org>");
        assert!(!k.known);
    }

    #[test]
    fn key_from_status_without_primary_uses_signing_key() {
        let fpr = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
        let s = format!("[GNUPG:] VALIDSIG {} 2026-11-01 1793000000 0 4 0 22 10 00\n", fpr);
        let k = key_from_status(&s).unwrap();
        assert_eq!(k.fingerprint, fpr);
        assert_eq!(k.uid, "(identité inconnue)");
    }

    #[test]
    fn key_from_status_none_when_unsigned() {
        assert!(key_from_status("[GNUPG:] BADSIG 1234 somebody\n").is_none());
    }

    #[test]
    fn describe_key_marks_first_time() {
        let k = SigningKey {
            fingerprint: "ABC".into(),
            uid: "Linus".into(),
            known: false,
        };
        assert!(describe_key(&k, true).contains("NOUVELLE"));
        assert!(describe_key(&k, false).contains("connue"));
    }

    /// Petit serveur HTTP local : sert une archive et sa signature, 404 pour
    /// le reste. Permet de tester le téléchargement (progression, fichier
    /// `.part`, signature obligatoire) sans dépendre de kernel.org.
    fn serve(listener: std::net::TcpListener, body: &'static [u8], sig: &'static [u8]) {
        std::thread::spawn(move || loop {
                let (mut stream, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => return,
                };
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let (code, payload): (&str, &[u8]) = if path.ends_with(".tar.xz") {
                    ("200 OK", body)
                } else if path.ends_with(".sign") {
                    ("200 OK", sig)
                } else {
                    ("404 Not Found", b"nope")
                };
                // `Connection: close` : une réponse par connexion, sinon ureq
                // réutilise la connexion et le test se bloque sur un second
                // `accept()`.
                let header = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    code,
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(payload);
                let _ = stream.flush();
        });
    }

    #[test]
    fn download_writes_archive_and_signature() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        static BODY: &[u8] = b"contenu-d-archive-factice";
        // Signature factice mais de forme réaliste (>100 o, en-tête PGP).
        static SIG: &[u8] = b"-----BEGIN PGP SIGNATURE-----\n\
\niQEzBAABCgAdFiEEq68RxlonCwMGEuM8eb4+QwBBGIYFAmV0ZXN0AAoJEHm+PkMAQRiG\n\
kQf9F0XQ0Y2m8Vw9m0nJ0v3kS2P0b1H4y+7oXe5cNwq9rT2uVb8dLm4pQ6yR1sT3vW5x\n\
=abcd\n-----END PGP SIGNATURE-----\n";
        serve(listener, BODY, SIG);

        let dir = std::env::temp_dir().join(format!("sekm-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rel = OrgRelease {
            version: "6.12.118".to_string(),
            moniker: "longterm".to_string(),
            eol: false,
            rc: false,
            tarball_url: format!("http://127.0.0.1:{}/linux-6.12.118.tar.xz", port),
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let (tar, sig) = download(
            &rel,
            &dir,
            &|_r, _t| {
                c2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            true,
        )
        .unwrap();
        let sig = sig.expect("signature requise");
        assert!(tar.exists());
        assert_eq!(std::fs::read(&tar).unwrap(), BODY);
        assert!(sig.exists(), "la signature doit être téléchargée aussi");
        assert!(
            sig.to_string_lossy().ends_with("linux-6.12.118.tar.sign"),
            "kernel.org signe le tar décompressé : {}",
            sig.display()
        );
        assert_eq!(std::fs::read(&sig).unwrap(), SIG);
        assert!(!dir.join("linux-6.12.118.tar.xz.part").exists(), "pas de .part résiduel");
        assert!(calls.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        // Deuxième appel : l'archive est déjà là, elle n'est pas re-téléchargée.
        let (tar2, _) = download(&rel, &dir, &|_, _| {}, true).unwrap();
        assert_eq!(tar, tar2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_refuses_release_without_signature() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        static BODY: &[u8] = b"archive-sans-signature";
        serve(listener, BODY, b"");

        let dir = std::env::temp_dir().join(format!("sekm-nosig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rel = OrgRelease {
            version: "6.12.999".to_string(),
            moniker: "manuel".to_string(),
            eol: false,
            rc: false,
            // Le serveur ne sert que `…/linux-6.12.118.tar.xz` : cette URL
            // pointe sur un 404, donc la signature est introuvable.
            tarball_url: format!("http://127.0.0.1:{}/linux-6.12.999.tar.xz", port),
        };
        let e = download(&rel, &dir, &|_, _| {}, true).unwrap_err();
        assert!(e.contains("signature"), "message attendu sur la signature : {}", e);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_key_is_isolated_per_home() {
        let tmp = std::env::temp_dir().join(format!("sekm-keys-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        let k = SigningKey {
            fingerprint: "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD".into(),
            uid: "Test <t@example.org>".into(),
            known: false,
        };
        assert_eq!(remember_key(&k, "6.12.118"), Ok(false));
        assert_eq!(remember_key(&k, "6.12.119"), Ok(true));
        let store = read_key_store();
        let rec = store.get(&k.fingerprint).unwrap();
        assert_eq!(rec.last_version, "6.12.119");
        assert_eq!(rec.uid, "Test <t@example.org>");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
