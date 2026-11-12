# cyclai-garmin

Plugin [CyclAI](../cyclai) pour **Garmin Connect** : il liste tes activités (vélo de route, home-trainer, course à pied) et télécharge le `.fit` d'origine de chacune, que CyclAI analyse comme n'importe quel fichier importé.

C'est un petit service HTTP, dans son propre conteneur. CyclAI l'appelle ; le plugin n'a **aucun accès** à CyclAI ni à sa base, et CyclAI n'a pas tes identifiants Garmin.

## Configuration

| Variable | Rôle |
|---|---|
| `GARMIN_API_KEY` | Clé API personnelle : Garmin Connect > Paramètres > API personnelle (expire après 30, 90, 180 ou 365 jours) |
| `CYCLAI_PLUGIN_SECRET` | Secret partagé avec CyclAI (16 caractères minimum), identique à `CYCLAI_PLUGIN_GARMIN_SECRET` côté CyclAI |
| `BIND_ADDR` | Adresse d'écoute, `0.0.0.0:8080` par défaut |

Chaque requête de CyclAI est signée (HMAC-SHA256, horodatage de 60 s au plus) : sans le secret, le plugin répond 401.

## Déploiement

```bash
docker build --build-context sdk=../cyclai/plugin-sdk -t cyclai-garmin .
```

Puis fusionne [docker-compose.example.yml](docker-compose.example.yml) dans le compose de CyclAI, et active le plugin dans CyclAI : **Paramètres > Sources d'activités**.

## Ce qui est importé

`cycling`/`road_biking` (Ride), `indoor_cycling`/`virtual_ride` (VirtualRide), `running`/`treadmill_running`/`track_running` (Run), `trail_running`/`ultra_run` (TrailRun). Le gravel, le VTT et les activités saisies à la main sont ignorés.

## Développement

```bash
cargo test
```

Le SDK du protocole est cherché dans `../cyclai/plugin-sdk`.
