<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# 46Anarchy Launcher

Launcher natif pour jouer sur le serveur **46Anarchy** (modpack Paladium, Minecraft 1.7.10, Forge). Connectez votre compte Microsoft, cliquez JOUER — le launcher installe et met à jour le jeu tout seul. Pas de mode hors-ligne : la connexion Microsoft est obligatoire. L'interface est en français.

## Prérequis

- Un compte Microsoft possédant Minecraft: Java Edition.
- Linux 64 bits, Windows 10/11 64 bits, ou macOS Apple Silicon.
- Aucun Java à installer : le runtime adapté est téléchargé automatiquement (Temurin 8 pour le modpack Paladium).

## Installation

### Linux : une seule commande

```sh
curl -fsSL https://raw.githubusercontent.com/46Anarchy/46Launcher/master/install.sh | bash
```

Le script détecte votre distro et installe le bundle correspondant depuis la dernière release GitHub. Seul `curl` est requis (`sudo` quand il faut installer un paquet système).

| Famille de distro | Bundle installé | Méthode |
| --- | --- | --- |
| Debian, Ubuntu, Linux Mint, Pop!_OS, elementary OS, Zorin OS, Kali, Raspbian | `.deb` | `apt-get install` (repli `dpkg -i`) |
| Fedora, RHEL, CentOS, Rocky Linux, AlmaLinux | `.rpm` | `dnf install` (repli `rpm -Uvh`) |
| Arch, Manjaro, Garuda, EndeavourOS, openSUSE, tout le reste | `.AppImage` | copié dans `~/.local/bin/46anarchy.AppImage` avec entrée desktop et icône, sans root |

Bureaux immuables/atomiques (Bazzite, Aurora, Bluefin, Silverblue, Kinoite, openSUSE Aeon/MicroOS, Vanilla OS, SteamOS, …) : toujours le `.AppImage`, sans root ni redémarrage.

Deux variantes CPU existent pour Linux x86-64 : `baseline` (tourne partout) et `x86-64-v3` (optimisée AVX2, CPU Intel Haswell / AMD Excavator et plus récents, ~2013+). Le script choisit automatiquement (`ARCH_VARIANT=v3|baseline|auto` pour forcer). Aperçu sans rien modifier :

```sh
curl -fsSL https://raw.githubusercontent.com/46Anarchy/46Launcher/master/install.sh | bash -s -- --dry-run
```

### Installation manuelle

Téléchargez le fichier depuis [Releases](https://github.com/46Anarchy/46Launcher/releases), puis :

```sh
sudo apt-get install ./46anarchy_*.deb   # famille Debian/Ubuntu
sudo dnf install ./46anarchy-*.rpm       # famille Fedora/RHEL
chmod +x ./46Anarchy_*.AppImage          # partout : portable, rien à installer
```

Les bundles `.deb`/`.rpm` apportent leur entrée desktop ; pour le `.AppImage`, le script d'installation l'écrit pour vous.

### macOS

Ouvrez le `.dmg` des Releases, glissez 46Anarchy dans `/Applications`. Build non signé : clic droit → Ouvrir au premier lancement (Gatekeeper). Pas de build Mac Intel pour l'instant.

### Windows

Lancez le `...-setup.exe` (installeur) ou le `.msi`.

### Désinstallation

```sh
sudo apt-get remove 46anarchy   # .deb
sudo dnf remove 46anarchy       # .rpm
# .AppImage : supprimez ~/.local/bin/46anarchy.AppImage,
# ~/.local/share/applications/46anarchy.desktop et l'icône
# ~/.local/share/icons/hicolor/512x512/apps/46anarchy.png.
```

Les fichiers du jeu (`~/.46anarchy`) sont conservés ; supprimez ce dossier pour tout effacer.

## Utilisation

- **Connexion Microsoft** : un code à valider sur microsoft.com/link, le launcher fait le reste. La session est conservée dans le keyring du système (aucun mot de passe stocké).
- **Multi-comptes** : ajout, actualisation et suppression depuis l'onglet Comptes ; changement rapide via le sélecteur en haut à droite.
- **JOUER** : installe ou met à jour si besoin (progression par fichier, annulable), puis lance le jeu. Le jeu continue de tourner si vous fermez le launcher. Le bouton passe à « En jeu » pendant la partie.
- **Mods** : le pack officiel est verrouillé ; vos propres `.jar` déposés dans `mods/` apparaissent dans l'onglet Mods et peuvent être activés/désactivés (recherche + filtres inclus).
- **Configuration Java** : mémoire allouée (curseur), presets d'arguments JVM par modpack, résolution et plein écran — tout est enregistré automatiquement, sans bouton.
- **Console et plantages** : sortie du jeu en direct dans l'onglet Console, cause du plantage affichée en cas de crash.
- **Serveur et patch notes** : statut du serveur 46Anarchy en direct sur l'accueil, notes de patch tirées des dépôts du projet.
- **Branche expérimentale** (Java 21, rendu modernisé) : sélectionnable depuis le sélecteur de modpack (« Expérimental modernisé ») ; OptiFine est mis de côté à la bascule et restauré au retour au classique.

## Mises à jour

À partir de la v0.2.0, le launcher vérifie sa propre version au démarrage : si une release plus récente est publiée, une fenêtre propose de l'installer, puis le launcher redémarre (Windows, macOS, Linux AppImage). Les paquets `.deb`/`.rpm` se mettent à jour avec la même commande que l'installation — `install.sh` installe toujours la dernière release — et les versions antérieures à la v0.2.0 aussi. Les fichiers du jeu (`~/.46anarchy`) ne sont jamais touchés par une mise à jour du launcher.

## Dépannage

- **Fenêtre vide ou noire (NVIDIA / pilotes récents)** : lancez une fois avec
  `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1` pour confirmer que c'est le rendu GPU.
- **Linux demande de déverrouiller le keyring au premier démarrage** : normal
  (gnome-keyring / kwallet), une seule fois — c'est là que vit votre session.
- **La connexion a expiré** : onglet Comptes → Actualiser le compte, ou
  retirez-le puis reconnectez-le.
- **Le jeu ne se lance pas après une mise à jour** : Accueil → Vérifier, qui
  recontrôle et répare les fichiers.
- Un bug, une idée : [Discord](https://discord.46anarchy.fr) ou
  [issues GitHub](https://github.com/46Anarchy/46Launcher/issues) (joignez si
  possible ce qu'affiche l'onglet Console).

## License

`AGPL-3.0-only`. See <https://www.gnu.org/licenses/agpl-3.0.html>.

Links: [46anarchy.fr](https://46anarchy.fr) ·
[discord.46anarchy.fr](https://discord.46anarchy.fr)
