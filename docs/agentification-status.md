# Agentification — statut (pivot 2026-07-25)

> **Pivot du 25/07** : l'approche « front séparé + proto `HydraAgent` réinventé »
> a été **jetée**. Le nouveau contrat agent est un **miroir de l'interface Go
> `engine.EngineClient`**, tunnelé en JSON sur gRPC. Voir aussi
> `architecture-agent-front.md` (partiellement périmé — se référer à ce fichier).

## Principe

La seule frontière data-plane est `engine.EngineClient` (la surface étroite que
`hoard.go`/`race.go` consomment via `e.client`). Un agent distant n'est rien de
plus qu'un `EngineClient` accessible par le réseau. Deux transports
interchangeables, choisis par une factory :

- **local** → `*ltclient.Client` (socket Unix vers Typhon, latence nulle,
  exactement comme aujourd'hui) ;
- **distant** → `*grpcclient.Client` (dial d'un HydraAgent).

Preuve à la compilation dans `internal/engine/factory.go` :
```go
var _ EngineClient = (*ltclient.Client)(nil)
var _ EngineClient = (*grpcclient.Client)(nil)
```
Le front (`api.Server` + HoardEngine/RaceEngine) ne branche jamais sur *où* une
session tourne : il appelle `SetClient(NewEngineClient(ep))`.

## Contrat gRPC — un tunnel JSON mince, pas un proto typé

`proto/hydra_agent.proto` = 2 RPC :
- `Call(CallRequest{engine, method, params}) → CallReply{result, error}` ;
- `Subscribe(SubscribeRequest{engine}) → stream EventFrame{payload}`.

`method` = nom snake_case d'une méthode d'`EngineClient` (registre
`internal/agentwire`). Les **params** ont des enveloppes JSON ; les **résultats**
sont les types `ltclient` marshalés *verbatim* → aucun message proto par type,
donc **zéro dérive** proto↔Go. Seul `add_torrent` embarque les octets du
`.torrent` (le chemin du front n'est pas lisible sur l'agent → l'agent
re-tempfile).

Auth = token bearer (ConstantTimeCompare, interceptors unary+stream) + TLS
optionnel (`--agent-tls-cert/-key` serveur, CA côté client) — re-portés de la nuit
du 24→25.

## Composants livrés (branche `oss-cleanup`)

| Package | Rôle |
|---|---|
| `internal/agentwire` | registre de méthodes + enveloppes de params JSON |
| `proto/hydra_agent.proto` → `internal/agentpb` | stubs gRPC (2 RPC) |
| `internal/agent` | `Server` : dispatch `Call` → `map[string]EngineClient` ; `Subscribe` gardé par `ownEvents` |
| `internal/engine/grpcclient` | `Client` : implémente `EngineClient` en tunnelant chaque appel |
| `internal/engine/factory.go` | `NewEngineClient(AgentEndpoint)` local\|grpc |
| `cmd/hydra` (`--agent-addr`) | ressert l'agent en **additif** (off par défaut) |
| `cmd/agentprobe` | sonde E2E manuelle |

## Garde-fou événements (`ownEvents`)

`SetEventHandler` est mono-slot sur un `ltclient` vivant. En mode **additif**
(l'agent partage les clients du monolithe), l'agent **ne doit pas** hijacker le
handler — sinon il vole les events au HoardEngine local. Donc `Subscribe` est
gardé par `ownEvents` (faux en additif → `codes.Unavailable`). Seul un agent
**dédié** (le front fait tourner la logique moteur ailleurs) met `ownEvents=true`
et possède le handler proprement. C'est l'état final du split.

## Validé E2E (staging :9099, TLS+token)

`agentprobe` dial race+hoard : `Ping`, `ListTorrents`, `GetSessionStats`
renvoient de vraies données moteur (race=2 torrents ul=198 Mo/dl=2,4 Go,
hoard=1). Propagation d'erreur moteur OK (`GetDiagnostics` renvoie l'erreur
Typhon telle quelle). Gate `Subscribe` actif.

## Reste (retrofit = basculement final)

1. **Consommer les agents dans `api.Server`** : remplacer
   `SetClient(proc.Client())` par `SetClient(NewEngineClient(ep))` piloté par un
   carnet d'agents en config. Généraliser l'agrégation race+hoard existante à
   N×agents (group-by info_hash pour le multi-home).
2. **Carnet d'agents en config** : `[[agent]]` (nom, transport, socket|addr,
   token, ca) + le concept **placement/strategy/save_path-par-agent** re-logé
   dans le **système de catégories existant** (pas un registre neuf).
3. **Mode agent dédié** : `--agent-only` (pas d'`api.Server`, `ownEvents=true`).
4. **Validation *arr réelle** contre un front multi-agent.
5. **hydra-relay** (companion VPS PROXY-v2+SOCKS5) = infra de l'agent, déjà
   livré (`79d9318`).
