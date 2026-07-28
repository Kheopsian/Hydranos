# Hydra — architecture distribuée agent / front (Phase C)

Statut : DESIGN (2026-07-24). Décisions arrêtées avec the operator. Transport agent↔front = gRPC.

## 1. Motivation

Aujourd'hui `hydra-go` = orchestrateur + front + hôte-moteur dans un seul process.
Objectif : brancher N moteurs (hoard/race) répartis sur plusieurs machines à un
seul plan de contrôle. Ex : 1 hoard en Allemagne, 1 en France, 1 en Suisse,
1 race en local, 1 GUI local qui pilote tout et à qui les *arr parlent.

Split en deux rôles (control-plane / data-plane) :
- **hydra-agent** (data-plane) : 1 par lieu. Héberge le(s) moteur(s) Typhon +
  toute la logique qui DOIT être co-localisée avec eux.
- **hydra-front** (control-plane) : 1 seul, local. GUI, shim qBit pour les *arr,
  agrégation, routage. Ne touche jamais un fichier ni un peer.

~90% du code Go actuel est re-homé, pas réécrit.

## 2. Frontières & protocoles (3 frontières, 3 protocoles)

| Frontière | Protocole | Pourquoi |
|---|---|---|
| **agent ↔ front** | **gRPC** (HTTP/2, protobuf) | contrat typé stable, streaming, mTLS/token, deadlines/backpressure |
| front ↔ navigateur | HTTP/REST + SSE (inchangé) | le browser n'est pas gRPC-friendly (grpc-web = proxy inutile ici) |
| front ↔ *arr | shim qBit HTTP (inchangé) | autobrr/Sonarr/Radarr attendent du HTTP qBit |
| agent ↔ Typhon | JSON-RPC NDJSON sur Unix socket (inchangé) | local, rapide, Typhon=Rust, aucun gain à changer |

gRPC = **uniquement la face sud de l'agent**. Pas de gRPC-web, pas de gRPC vers Typhon.

## 3. Qui vit où

**Agent (co-localisé, dépend de l'IP/du disque local) :**
- moteurs Typhon (race + hoard)
- **scheduler d'annonce** (l'egress compte : l'agent DE annonce DEPUIS l'Allemagne, son propre VPS/FOU)
- choking, disk I/O, slot manager, drain, tracker announce
- `state.json` par agent, resume/fastresume par agent
- data payload (vit à côté du moteur)

**Front (control-plane) :**
- GUI + SSE vers le navigateur
- shim qBit pour les *arr
- agrégation (group-by-hash des listes des N agents)
- routage / placement (un *arr add → quel agent ?)
- registre des catégories (source de vérité)
- collecte/agrégation bench, dedup cross-agent

## 4. Contrat gRPC (esquisse `.proto`)

```proto
service HydraAgent {
  rpc GetAgentInfo(Empty) returns (AgentInfo);        // name, data_roots, version, caps
  rpc Ping(Empty) returns (Pong);

  // Contrôle (unary) — miroir de RaceEngine/HoardEngine
  rpc AddTorrent(AddRequest) returns (AddReply);
  rpc RemoveTorrent(RemoveRequest) returns (Empty);
  rpc StartTorrent(Ref) returns (Empty);
  rpc StopTorrent(Ref) returns (Empty);
  rpc VerifyTorrent(Ref) returns (Empty);
  rpc SetCategory(SetCategoryRequest) returns (Empty);

  // Lecture
  rpc ListTorrents(ListRequest) returns (TorrentSnapshot);   // full snapshot (resync)
  rpc GetTorrent(Ref) returns (TorrentStatus);
  rpc GetSessionStats(Empty) returns (SessionStats);

  // Streaming — lifecycle + snapshots stats (remplace le snapshot pusher SSE actuel)
  rpc SubscribeEvents(SubscribeRequest) returns (stream AgentEvent);
}
```

**Auth** : token en per-RPC metadata sur TLS par défaut (bootstrap facile pour un
user OSS) ; mTLS en option (chaque agent+front a un cert) pour les parano.
Pas de dépendance à un underlay wireguard/FOU.

**Reconnect/resync** : un stream gRPC meurt sur blip réseau. À la (re)connexion le
front fait `ListTorrents` (snapshot full) puis `SubscribeEvents` (delta). C'est
exactement le pattern *snapshot + delta* déjà en place en SSE — on le remonte
d'un cran : agent gRPC-stream → front agrège → ré-émet en SSE vers le navigateur.

## 5. Identité & multi-home

- Identité torrent : **`info_hash → Set<agent>`** (un torrent peut vivre sur N agents).
- Le multi-seed est **natif BitTorrent** : les agents ne coordonnent RIEN au niveau wire.
- Multi-home réel = stats HONNÊTES (chaque agent uploade vraiment) — ≠ l'ancien bug
  secondary_stats (fantôme). Le seul risque = **règles tracker privé** (multi-seedbox
  autorisé ?) → **warning dismissible** (les gens qui font du multi-client sont conscients).
- `secondary_stats` (dual-stack VPS) devient **par-agent** sur les trackers qui somment par peer_id.
- **Réplication data = HORS-SCOPE v1** : on assume la data déjà présente sur l'agent (comme le seed-existing d'aujourd'hui).

## 6. Modèle de catégories (B2, per-agent)

Une catégorie = objet **front** :
```
Category {
  name        string
  mode        "race" | "hoard"
  placement   []agent | policy   // quels agents hébergent les nouveaux torrents de cette cat
  save_path   map[agent]path     // dérivé <agent.data_root>/<name> par défaut, override par (cat,agent)
}
```
- **Dégénère proprement en mono-agent** : 1 agent "local" → 1 seul save_path. C'est
  l'état actuel. En multi-agent la map grossit. **Zéro rework entre mono et multi.**
- On-disk **forward-compatible** : garder le `save_path` plat (= agent local/défaut),
  ajouter un `agents: {agentName: path}` optionnel. Résolution : `agents[a]` sinon `save_path` plat.
- **Placement** : nécessaire parce qu'un *arr add via le shim porte SEULEMENT la
  catégorie (pas d'agent) → c'est la politique de placement de la catégorie qui dit
  au front où router.
- Changer un save_path n'affecte que les **nouveaux** torrents (pas de move auto — comme qBit).

## 7. Plan de PRs (staged)

1. **Schéma catégorie forward-compatible** (per-agent map optionnelle, back-compat, helper de résolution). Faisable MAINTENANT, mono-agent, testable. ← on commence par là.
2. Définir le `.proto` + génération (buf/protoc) ; serveur gRPC agent qui wrappe les moteurs existants ; front parle en loopback d'abord (tout local).
3. Extraire les 2 binaires `hydra-agent` (data-plane) / `hydra-front` (control-plane), toujours en local.
4. Transport réseau + auth (TLS/token), 1 agent distant.
5. Agrégation + routage dans le front (group-by-hash, placement).
6. Identité multi-home (`Set<agent>`), secondary_stats per-agent, warning.
7. Durcissement failover/reconnect (agent down → front dégradé, *arr continuent sur les autres).

## 8. Points durs (tous côté agent↔front)

- **Modèle de panne** : aujourd'hui rien ne tombe (tout local). Demain un agent part
  en vrac → le front doit rester réactif (pull par-agent avec timeout, merge ce qui
  répond), les *arr continuent sur les autres, les add vers l'agent mort échouent/queue.
- **Latence** : IPC sub-ms → 20-40ms vers l'Allemagne. Front async partout, jamais bloquer le GUI sur un agent lent.
- **Agrégation** : group-by-hash + merge des stats (pas une simple concat), fan-out sur delete/pause.
