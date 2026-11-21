#!/bin/bash
# Tests de non-régression de Solus External Kernel Manager.
#
# Couvre ce qui ne se voit pas dans une revue de code :
#   1. tests unitaires (parsers, config noyau, chemins, TOFU, sécurité) ;
#   2. construction du binaire ;
#   3. rendu de CHAQUE onglet sans affichage (un layout droppé ou une page
#      vide n'échoue jamais à la compilation ni à l'exécution) ;
#   4. CLI kernel.org : liste des versions, RC présents, version invalide
#      refusée ;
#   5. absence de fuite de signaux (QObject::disconnect) au démarrage.
#
# Usage : tests/nonregression.sh        (depuis n'importe où)
set -u
cd "$(dirname "$0")/.." || exit 2

fail=0
step() { printf '\n== %s ==\n' "$1"; }
check() { if [ "$1" -eq 0 ]; then echo "  OK    $2"; else echo "  ECHEC $2"; fail=1; fi; }
neg()   { if [ "$1" -ne 0 ]; then echo "  OK    $2"; else echo "  ECHEC $2"; fail=1; fi; }

LOGDIR=${TMPDIR:-/tmp}/sekm-nonreg
mkdir -p "$LOGDIR"

step "Tests unitaires"
cargo test --release --quiet >"$LOGDIR/unit.log" 2>&1
check $? "cargo test --release ($(grep -c 'test result: ok' "$LOGDIR/unit.log") suite(s), $(grep -oE '[0-9]+ passed' "$LOGDIR/unit.log" | head -1))"

step "Binaire"
cargo build --release --quiet >"$LOGDIR/build.log" 2>&1
check $? "cargo build --release"
BIN=target/release/solus-external-kernel-manager
[ -x "$BIN" ] || { echo "  ECHEC binaire absent"; exit 1; }

step "Interface : un démarrage par onglet (0..4)"
for t in 0 1 2 3 4; do
    QT_QPA_PLATFORM=offscreen SEKM_TAB=$t SEKM_SMOKE_MS=800 "$BIN" >"$LOGDIR/tab$t.log" 2>&1
    rc=$?
    check $rc "onglet $t démarre et sort proprement (exit $rc)"
    if grep -qi "QObject::disconnect" "$LOGDIR/tab$t.log"; then
        echo "  ECHEC fuite de signaux sur l'onglet $t"; fail=1
    else
        echo "  OK    onglet $t sans fuite de signaux"
    fi
done

step "CLI kernel.org"
"$BIN" --kernelorg-list >"$LOGDIR/list.log" 2>&1
check $? "--kernelorg-list"
grep -q longterm "$LOGDIR/list.log"; check $? "la liste contient des versions longterm"
grep -q -- "-rc" "$LOGDIR/list.log"; check $? "les versions -rc restent proposées"
grep -q "non signé" "$LOGDIR/list.log"; check $? "les -rc sont annoncés comme non signés"

"$BIN" --kernelorg-stage zzz >"$LOGDIR/badver.log" 2>&1
neg $? "version invalide refusée (zzz)"

"$BIN" --kernelorg-stage 6.12.118 --base nonbase --build-root "$LOGDIR/kb" >"$LOGDIR/badbase.log" 2>&1
neg $? "base de .config inconnue refusée"

step "Note"
echo "  (la compilation réelle d'un noyau n'est pas couverte : trop longue."

printf '\n'
if [ "$fail" -eq 0 ]; then echo "NON-REGRESSION : OK"; else echo "NON-REGRESSION : ECHECS (journaux dans $LOGDIR)"; fi
exit "$fail"
