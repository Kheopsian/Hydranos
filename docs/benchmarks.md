# Comparatif : six clients BitTorrent servant 50 000 torrents

Mesures faites sur Orion (128 cœurs, 251 Gio, Linux 6.12) les 17 et
18 septembre 2026. Méthode complète, données brutes et scripts d'analyse :
[bt-engine-bench](https://github.com/Kheopsian/bt-engine-bench). Tous les
chiffres de cette page se recalculent depuis les CSV archivés dans
`runs/` de ce dépôt.

## Ce qui est mesuré

Tenir 50 000 torrents ne coûte presque rien : ils dorment dans une map,
et n'importe quel moteur écrit depuis vingt ans en est capable. Mesurer
ça et appeler ça un comparatif de passage à l'échelle, c'est refaire la
promesse des clients de 2010 qui affichaient dix millions de torrents
sans être capables d'en servir un.

Le protocole est donc un client **qui sert** :

- la charge utile de chaque torrent est sur le disque et le client la
  seede (ceux qui proposent un mode « fais confiance au disque »
  l'utilisent) ;
- **la file d'attente de chaque client est coupée** (`queueing_enabled=false`,
  `max_active_*=-1`, `download-queue-enabled=false`), donc les 50 000
  sont vivants et non garés ;
- **64 pairs synthétiques** tournent en permanence sur tout le catalogue :
  connexion, handshake sur l'infohash, `interested`, demande de toutes
  les pièces, **vérification SHA-1 de chaque pièce reçue**, déconnexion,
  torrent suivant ;
- le tracker compte les annonces de chaque client ;
- après 3 h 30, le client est arrêté puis redémarré avec son catalogue
  sur disque, et chaque étape est chronométrée.

Les pairs sont les nôtres parce qu'aucun vrai client ne tient 50 000
torrents ouverts en leecher, et que ceux qui essaient apportent leurs
propres pannes dans la mesure. Mesurer côté pair a un autre avantage :
le débit ne dépend pas de l'honnêteté du client testé. Là où les deux
chiffres existent, ils coïncident — Transmission déclarait 100 480 816
octets envoyés pendant que les pairs en avaient reçu 95,8 Mio.

RAM, CPU, threads et descripteurs sont lus dans `/proc` sur **l'arbre de
processus** du client. Ni `docker stats` (qui compte le page cache : le
piège qui nous avait fait lire des dizaines de Go pour un processus de
quelques centaines de Mo), ni la comptabilité interne du client (qui est
de l'allocateur, pas du résident).

## Résultats

### Service réel — 50 000 torrents servis pendant 3 h 30

| client | servi | débit | sessions/s | sessions refusées | pièces corrompues |
|---|---|---|---|---|---|
| Deluge | 96.5 Gio | 7.9 Mio/s | 31.4 | 0.0 % | 0 |
| rTorrent | 90.9 Gio | 7.4 Mio/s | 29.6 | 0.3 % | 0 |
| qBittorrent | 88.0 Gio | 7.2 Mio/s | 28.8 | 5.6 % | 0 |
| Hydranos | 80.8 Gio | 6.6 Mio/s | 26.4 | 1.1 % | 0 |
| Transmission | 19.6 Gio | 1.6 Mio/s | 6.4 | 0.0 % | 0 |
| rqbit | 4.4 Gio | 0.3 Mio/s | 1.4 | 92.1 % | 0 |

### Coût pendant ce service

| client | torrents tenus | RSS | Kio/torrent | CPU | threads | fds | dérive mémoire | annonces |
|---|---|---|---|---|---|---|---|---|
| Deluge | 50000 | 2462 Mio | 50.4 | 0.24 cœur | 22 | 193 | +35 Mio/h | 21827 |
| rTorrent | 50000 | 983 Mio | 20.1 | 0.44 cœur | 3 | 274 | +22 Mio/h | 424215 |
| qBittorrent | 50000 | 2252 Mio | 46.1 | 0.40 cœur | 29 | 265 | +104 Mio/h | 21196 |
| Hydranos | 50000 | 877 Mio | 18.0 | 0.12 cœur | 86 | 10030 | +147 Mio/h | 402963 |
| Transmission | 50000 | 704 Mio | 14.4 | 0.08 cœur | 14 | 218 | +1 Mio/h | 379988 |
| rqbit | 3578 | 358 Mio | 102.3 | 0.90 cœur | 130 | 3609 | +10 Mio/h | 28608 |

### Reste-t-il utilisable ? (lister ses 50 000 torrents, pendant le service)

| client | médiane | pire |
|---|---|---|
| rqbit | 0.02 s | 0.05 s |
| Transmission | 0.29 s | 2.40 s |
| Hydranos | 2.10 s | 14.02 s |
| Deluge | 3.37 s | 6.56 s |
| qBittorrent | 13.21 s | 35.80 s |
| rTorrent | 182.68 s | 300.00 s |

### Arrêt et redémarrage avec le catalogue sur disque

| client | arrêt | API de retour | catalogue retrouvé |
|---|---|---|---|
| Deluge | > 10 min (tué) | non mesuré | non mesuré |
| rTorrent | 4.5 min | 27.9 min | 50000 / 50000 |
| qBittorrent | 3.4 min | 43.9 s | 50000 / 50000 |
| Hydranos | 5.1 s | 11.9 s | 49894 / 50000 (abandon) |
| Transmission | 8.5 s | 29.1 s | 50000 / 50000 |
| rqbit | 2.2 s | 1.4 s | 0 / 3578 (abandon) |

### Ce que le client garde de ce qu'il a accepté

| client | ajouts acceptés | torrents tenus |
|---|---|---|
| Deluge | 50000 | 50000 |
| rTorrent | 49976 | 50000 |
| qBittorrent | 50000 | 50000 |
| Hydranos | 49947 | 50000 |
| Transmission | 49992 | 50000 |
| rqbit | 49997 | 3578 |

Le run de qBittorrent a dû être refait : Hydranos binde `listen_port` **et**
`listen_port+1`, avait donc pris le port de qBittorrent, et les pairs
passaient 3 h 30 à se faire refuser par le mauvais processus. rTorrent a
servi de témoin sur les deux passages — 88,6 Gio puis 90,9 Gio, soit
2,6 % d'écart — ce qui autorise à lire la relance à côté des autres.

## Ce que ça dit

**Quatre clients sur six servent à peu près au même débit** (7 à 8 Mio/s
ici), et ce débit est plafonné par le générateur de charge, pas par eux.
Ce qui les sépare, c'est ce que ça leur coûte et ce qu'il reste
d'utilisable pendant ce temps :

- **rTorrent** sert le mieux avec 3 threads et 20 Kio/torrent, mais son
  interface de contrôle est morte : **183 s en médiane** pour lister ses
  propres torrents, 300 s au pire (= notre plafond). Un front-end
  branché dessus est inutilisable.
- **qBittorrent tient le choc** — 88 Gio servis, 7,2 Mio/s — contrairement
  à ce qu'on attendait. Le prix est ailleurs : **2,2 Gio de RAM**
  (46 Kio/torrent, 2,5× Hydranos), **13 s pour lister** ses torrents, et
  **3,4 min pour s'arrêter**.
- **Deluge** sert le plus (96 Gio) mais au prix le plus élevé :
  **2,4 Gio de RAM**, et il **ne s'arrête pas en moins de dix minutes** —
  notre plafond l'a tué avant qu'il ait fini.
- **Transmission** est le plus sobre (704 Mio, 0,08 cœur) et le plus
  réactif (0,3 s pour lister, 8,5 s pour s'arrêter, catalogue complet au
  retour), mais il sert **cinq fois moins** : 1,6 Mio/s, six sessions par
  seconde. Il rationne ses pairs.
- **rqbit s'effondre** : 92 % des sessions refusées, 0,3 Mio/s, et il ne
  tenait que 3 578 des 50 000 torrents qu'il avait acceptés.
- **Hydranos** est le moins cher par torrent (18 Kio) et de loin le moins
  gourmand en CPU (0,12 cœur pour 6,6 Mio/s servis), avec un arrêt en
  5 s. Deux réserves, à corriger : **+147 Mio/h de dérive mémoire** sous
  charge de service — la plus forte du lot avec qBittorrent — et
  **10 030 descripteurs ouverts** contre 200 à 300 pour les autres.

## ⚠️ Correction d'un chiffre qu'on a publié

Le dépôt et le wiki annonçaient **« libtorrent monte à 640 Ko/torrent à
20 000, super-linéaire, jusqu'à l'OOM »**. Ce chiffre ne tient pas.

Mesuré ici, qBittorrent tient **46,1 Kio/torrent en servant 50 000
torrents**, et 22,5 Kio/torrent au repos — sans effondrement sur trois
heures et demie. L'ancien résultat venait de trois défauts cumulés :

1. **Le banc était la charge.** Un échantillon coûtait un appel à
   Typhon, un dump JSON complet à qBittorrent, et **deux appels XML-RPC
   par torrent** à rTorrent — 100 000 appels par échantillon à 50 000.
2. **Rien n'était enregistré pendant le chargement**, là où les clients
   souffrent le plus.
3. **L'intervalle d'annonce était de 30 s**, soit 1 600 annonces par
   seconde à 50 000 torrents. Aucun tracker réel ne concède ça.

Le tiers qui avait mesuré 175 Ko/torrent sur sa machine était plus près
de la réalité que nous. **Le claim des 640 Ko doit être retiré.**

## ⚠️ Pourquoi l'expérience évidente ne dit rien

Avant ça, le protocole était « ajouter 50 000 torrents et regarder ». Le
résultat était un tableau bien rangé où tout le monde s'en sortait — et
il ne valait rien, parce que presque personne ne faisait quoi que ce soit.

Avec ses réglages par défaut, qBittorrent avait mis **49 995 des 50 000
torrents en `queuedDL`** : cinq actifs, et **zéro annonce en trois heures
et demie**. Hydranos fait l'équivalent (dix slots de téléchargement à la
fois). Deluge et Transmission ont leurs propres files. rTorrent était le
seul à traiter le catalogue comme vivant.

| client | au repos | en service |
|---|---|---|
| Hydranos | 4,9 Kio/torrent | 18,0 Kio/torrent |
| Transmission | 9,2 | 14,4 |
| rTorrent | 18,4 | 20,1 |
| qBittorrent | 22,5 | 46,1 |
| Deluge | 49,6 | 50,4 |

Servir double à quadruple le coût par torrent des clients légers et ne
bouge presque pas celui de Deluge. Le chiffre « au repos » mesure surtout
la qualité du rangement.

## 🚨 rqbit perd des torrents sur ajouts concurrents, et répond « succès »

1 000 torrents ajoutés **8 en parallèle** : 1 000 ajouts acceptés, **zéro
erreur**, et le client en garde **144**. Les mêmes 1 000 ajoutés **un par
un** : **1 000 gardés**. Son API confirme le chiffre bas.

À plus grande échelle, la proportion bouge : 306 sur 2 000, 3 578 sur
50 000. Après redémarrage, il revient avec **aucun** d'entre eux.
L'appel d'ajout renvoie un succès à chaque fois : rien sur quoi
réessayer, et un catalogue silencieusement réduit.

## Les politiques d'annonce n'ont rien à voir entre elles

Annonces reçues par le tracker en 3 h 30 pour le même catalogue de
50 000 torrents : **rTorrent 424 215**, **Hydranos 402 963**,
**Transmission 379 988**, **rqbit 28 608**, **Deluge 21 827**,
**qBittorrent 21 196**.

Un facteur vingt entre le plus bavard et le plus discret. Un client qui
annonce 50 000 torrents sur un intervalle de 30 minutes demande
**28 requêtes par seconde** à son tracker, en permanence — et c'est
invisible pour qui ne mesure que la RAM.

## Limites

- **Un seul hôte, un seul jeu de torrents** : 50 000 torrents
  synthétiques de 256 Kio (4 pièces de 64 Kio). La RAM par torrent est
  dominée par le nombre de pièces ; comparer des Kio/torrent entre deux
  jeux différents n'a pas de sens, entre deux clients sur le même jeu, si.
- **Pas de vrai essaim Internet** : pairs synthétiques en loopback, pas
  de NAT, pas de latence, pas de chiffrement, DHT et PEX coupés partout.
- **Le débit est plafonné par le générateur** (64 pairs, 2 s de pause
  entre deux torrents). Les 7-8 Mio/s ne sont pas une limite des clients.
- **Mode seed** pour ceux qui savent le faire : le picker de pièces de
  libtorrent en mode seed est minimal, donc ces clients sont mesurés
  moins chers qu'en téléchargement. Tous ceux qui savent le faire le
  reçoivent.
- **Un run par client** : pas de moyenne. Les écarts d'un facteur deux
  sont significatifs, ceux de 10 % ne le sont pas.
- Deux mesures sont des plafonds, pas des valeurs : l'arrêt de Deluge
  (> 10 min) et le listing de rTorrent au pire cas (300 s).
