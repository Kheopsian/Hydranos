# Hydra — API HTTP

Deux interfaces sur le même daemon. **Lis ça avant de deviner un endpoint.**

## Instances
| Compte | URL | Container |
|---|---|---|
| A (prod, Kheopsian) | `http://localhost:8199` | `hydra-go` (net=container:styx) |
| B (compte 2, pur-seeder) | `http://localhost:8299` | `hydra-b` |

## Auth
- **Natif `/api/*`** : header `X-API-Key: change-me-in-production`
- **Shim qBit `/api/v2/*`** : stateless. `POST /api/v2/auth/login` (n'importe quels creds → cookie `SID`). La plupart des calls passent même sans.

---

## ⭐ API NATIVE `/api/*` — LA vraie interface, à utiliser par défaut
Auth = `X-API-Key`.

**Torrents**
- `POST /api/torrents/upload` — **ajoute un NOUVEAU torrent à DL+seed**. multipart : `mode=hoard|race`, `save_path`, `category`, fichier `.torrent`. (Le farm sw_fill l'utilise en `mode=hoard`.)
- `POST /api/torrents` — add (magnet/url).
- `DELETE /api/torrents/:info_hash` — **retire des DEUX moteurs** (≠ purge race-only).
- `POST /api/torrents/:info_hash/reannounce`
- `POST /api/torrents/:info_hash/add-tracker`
- `GET /api/torrents/:info_hash/files` — contenu du torrent : `files[]` (`path`, `size`), cherché dans le moteur qui le détient. Ajoute `availability` (`min`, `max`, `avg`, `num_pieces`) **uniquement si le torrent a une piece map**, c.-à-d. en mode download — un torrent seed_mode n'a pas de bitfield (c'est ce qui rend 100k torrents pas chers), donc la clé est absente et l'UI affiche « n/a ».
- `GET /api/opt/flags` / `POST /api/opt/flags` — flags d'optim à chaud. En plus des flags Go (`ipc_route`, `ipc_frame`, `list_cache`, `ipc_prealloc`, `qbit_snapshot`, `totals_cache`, `gogc`, `list_cache_ttl_ms`), deux flags **moteur** (Rust), appliqués aux DEUX moteurs et rendus sous `engine_flags.{race,hoard}` :
  - `session_pinning` (bool) — thread-per-core : épingle chaque session de pair sur un runtime mono-thread. ⚠️ **Ne s'applique qu'aux NOUVELLES sessions** : une bascule met des minutes à prendre effet, le temps que les pairs tournent. Blocs d'A/B assez longs pour survivre à ça.
  - `session_runtimes` (`value`, ≥1) — taille du pool. **Refusé (400) une fois le pool construit** (= après le premier `session_pinning:true`) : démonter des runtimes qui portent des sessions vivantes n'est pas le rôle d'un bouton de mesure. Défaut = `TYPHON_SESSION_RUNTIMES`, sinon 1/cœur.


**État / stats**
- `GET /api/status` — état global (baseline, hoard{...}, day_uploaded...).
- `GET /api/hoard/torrents` — **liste hoard complète** (info_hash, name, state, progress, save_path, total_size, total_upload, swarm_seeds, tracker_error, tracker_error_msg...). LA source pour auditer.
- `GET /api/race/torrents` — liste race.
- `GET /api/stats/baseline`
- `GET /api/events` — SSE push.

**Race-only (handoff/drain)**
- `POST /api/race/torrents/:info_hash/purge?delete_files=` — **retire du SEUL moteur race** (garde le hoard). À préférer au DELETE quand dual-seed.

**Catégories** : `GET/POST /api/categories`, `PUT/DELETE /api/categories/:name`
  - `GET /api/categories/orphans` -> `[{name, torrents}]` : labels portés par des torrents mais ne correspondant à aucune catégorie configurée (résidus de suppressions antérieures au nettoyage durable du label).
  - `DELETE /api/categories/:name` -> `{cleared, cleared_stored, was_orphan}` : retire l'entrée de `categories.json`, efface le label dans les DEUX moteurs et dans le store SQLite. Accepte un label orphelin (absent de la liste) ; **404 seulement si aucun torrent ne le porte non plus**.
**Override passkey (2.7.10+)** : `GET /api/announce/passkeys` ; `POST /api/announce/passkeys` `{"host":"tracker.torr9.net","passkey":"..."}` (vide = clear). Hot, pas de restart, par-tracker.
**Divers** : `GET /api/public-ip`, `/api/fs/browse`, `/api/peers/top`, `/health`, `/metrics`.

---

## ⚠️ SHIM qBit `/api/v2/*` — couche de COMPAT (autobrr/cross-seed). À n'utiliser QUE si nécessaire.
La **SEULE** raison légitime de l'utiliser : **seeder de la DATA DÉJÀ SUR DISQUE** sans re-DL (`skip_checking=true` → `AddTorrentSeedMode`). Le natif `verify/recheck` stall sur de la data existante.

- `POST /api/v2/app/setPreferences` — form `json={"listen_port":51413}` (chaîne acceptée aussi, corps JSON brut aussi). **Seul `listen_port` est appliqué** (rebind du moteur race + persisté) ; les autres clés sont acceptées et ignorées comme le fait qBit. 500 si le rebind échoue — le port annoncé est toujours le port réellement bindé.
- `POST /api/v2/torrents/add` — multipart : `torrents=@file`, `savepath`, `skip_checking=true`, `paused=false`, `category=<cat>`.
  - ⚠️ **ROUTING PAR CATÉGORIE** : le `mode` de la catégorie (`categories.json`) décide race vs hoard. **Catégorie inconnue → défaut RACE** (piège classique). Utiliser une catégorie `mode=hoard` (`movies`, `Calewood`, `caleB`...).
  - ⚠️ **`savepath` = le chemin CONTENU EXACT** (le dossier qui contient le top-level du torrent) = le `save_path` que l'API reporte pour le même torrent côté A. PAS le parent. Pour un single-file-in-folder, c'est le dossier release.
  - ⚠️ **Lag stats ~25s** : juste après l'add, `total_size`/`progress` = 0 (verify-throttle mappe les fichiers). `seeding prog=1.0` apparaît après. NE PAS conclure à un échec avant ~30s.
- `POST /api/v2/torrents/delete` : `hashes=<ih>&deleteFiles=false` (⚠️ `false` pour garder la data).
- Autres : `/torrents/info`, `/pause`, `/resume`, `/setCategory`, `/auth/login`.

---

## Règle d'or (le truc que je me plante dessus)
| Besoin | Endpoint |
|---|---|
| **Nouveau** torrent (DL puis seed) | natif `POST /api/torrents/upload` `mode=hoard` |
| **Seeder data existante** (pas de re-DL) | shim `POST /api/v2/torrents/add` `skip_checking=true` + catégorie hoard |
| Lister / auditer | natif `GET /api/hoard/torrents` |
| Retirer une copie race (dual-seed) | `POST /api/race/.../purge` (PAS le DELETE qui vire les 2) |

---

## 🗺️ CARTE COMPLÈTE DES ROUTES (dérivée de `internal/api/routes_hydra.go` + `routes_qbit.go`)
> Les sections au-dessus = highlights curés. Ci-dessous = **exhaustif**. Si un endpoint n'est pas ici, il n'existe pas — ne le devine pas. Source de vérité = le code ; régénérer cette table quand on ajoute une route.

**Racine (hors `/api`, pas d'auth)** : `GET /health` · `GET /metrics` · `GET /api/startup`

**`/api/*` — natif, auth `X-API-Key`**

| Groupe | Routes |
|---|---|
| *(top-level)* | `POST /torrents` · `POST /torrents/upload` · `DELETE /torrents/:ih` · `POST /torrents/:ih/reannounce` · `POST /torrents/:ih/add-tracker` · `GET /torrents/:ih/files` · `GET /status` · `GET /events` (SSE) · `GET /public-ip` · `GET /fs/browse` · `GET /health/anomalies` |
| announce | `GET/POST /announce/passkeys` · `GET/POST /announce/clients` |
| categories | `GET /categories` · `GET /categories/orphans` · `POST /categories` · `PUT /categories/:name` · `DELETE /categories/:name` |
| peers | `GET /peers/top` · `GET /peers/seedboxes` |
| stats | `GET/POST /stats/baseline` |
| config | `GET/POST /config/create-folder` |
| port-forward | `GET /port-forward` · `GET /port-forward/assignment` · `POST /port-forward/assignment` |
| **`/api/race`** | `GET /torrents` · `GET /torrents/:ih` · `GET /choking` · `GET/POST /settings` · `POST /uploader` · `GET /uploaders` · `GET /uploaders/:username` · `GET /timeline/:ih` · `POST /torrents/:ih/purge` · `POST /listen-port` |
| **`/api/hoard`** | `GET /stats` · `GET /torrents` · `GET /torrents/:ih` · `POST /pause-all` · `POST /resume-all` · `POST /restart-stuck` · `POST /verify-downloading` · `POST /torrents/:ih/verify` · `POST /torrents/:ih/category` · `GET/POST/DELETE /download-slots` · `POST /listen-port` |
| **`/api/hardlinks`** | `GET /summary` · `POST /scan` · `GET /orphans` · `GET /orphans/:ih/files` · `GET/POST /config` · `GET /orphan-media` · `GET /ghosts` · `POST /cleanup` · `POST /relink` · `GET /superseded` |
| **`/api/drain`** | `GET /status` · `GET /history` · `POST /now` |
| **`/api/huntarr`** | `GET /status` · `GET /history` · `GET/POST /config` · `GET /library` · `GET /grabs` · `GET /found` · `POST /scan` |
| **`/api/arr-cleanup`** | `GET /scan` · `POST /execute` |
| **`/api/benchmark`** | `GET /current` · `GET /range` · `GET /compare` · `GET /race-events` · `GET /race-snapshots/:ih` |
| **`/api/vpn-speedtest`** | `GET /latest` · `GET /history` · `POST /run` |

**`/api/v2/*` — shim qBit** (compat only, cf section dédiée) : `POST /auth/login` · `POST /auth/logout` · `POST /app/setPreferences` · `POST /torrents/add` · `POST /torrents/delete` · `POST /torrents/pause` · `POST /torrents/resume` · `POST /torrents/setCategory` · `GET /torrents/info` · `GET /torrents/categories` · `POST /torrents/createCategory` · `POST /torrents/editCategory` · `POST /torrents/removeCategories`

### Download slots (sélecteur hoard) — `enforceDownloadSlots` (`internal/engine/hoard.go`)
- `active_downloads` = N slots de DL simultané. Le reste des torrents incomplets est **parké** (stop).
- Sélection = **seeds descendants** (mieux seedé = finit + vite) + une **probe quota** (`maxSlots/5`) réservée aux « jamais servis » (anti-catch-22 seeds=0). Phase 1 = **progress-demote** avec cooldown/backoff si pas de progrès dans la fenêtre.
- `GET/POST/DELETE /api/hoard/download-slots` = override **global** du nombre de slots (`SetDownloadSlotsOverride`), PAS un pin par-torrent.
- ✅ **Pin par-torrent (2.9.3)** : `POST /api/hoard/torrents/:ih/pin` · `POST /api/hoard/torrents/:ih/unpin` · `GET /api/hoard/pinned`. Un torrent pinné entre dans le target-set en PREMIER (hors seed-sort, ignore le cooldown) et est exempté du progress-demote. Persisté dans `<dataDir hoard>/hoard_pinned.json` (= `<dataDir>/hoard/hoard_pinned.json`, survit aux restarts). Le pin vide le slotProgress pour un start immédiat au prochain tick. Cas d'usage = grabs `releases_src` délibérés (BDMV, raretés) qu'on veut peu importe la santé du swarm (ex JJK S2 BD à 1-3 seed).

---

## torr9 (API externe, ≠ Hydra)
- Base `https://api.torr9.net/api/v1`. Bearer = `/tmp/token.txt`. Passkey = `/tmp/passkey.txt`.
- `GET /users/me` → profil (jeton_balance, passkey, total_*_bytes).
- `GET /torrents/search?uploader=<username>&limit=100&page=N` → **filtre uploader OK** (`total_count`, `total_pages`, `torrents[]`). (≠ `/torrents?...` qui n'expose pas `uploader` et cap à 20.)
- `GET /torznab/torrents/{id}/download?passkey=<pk>` → `.torrent` avec la passkey embarquée dans l'announce.
