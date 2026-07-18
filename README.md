# atlapool

Atlassian credential proxy (Jira + Confluence) with per-agent allowlists.

> Status: v1 scaffold — the endpoints below run, but MCP/REST proxy features are implemented in follow-up issues.

## Quick start

```sh
cp config.example.toml config.toml
# Edit config.toml with your Atlassian base URL and token reference.

cargo run
# Listening on 0.0.0.0:8080

curl http://localhost:8080/health
curl http://localhost:8080/stats
```

## Docker

```sh
docker build -t atlapool .
docker run -p 8080:8080 -e PORT=8080 atlapool
```

## Configuration

See [config.example.toml](config.example.toml) for all options.

## License

MIT
