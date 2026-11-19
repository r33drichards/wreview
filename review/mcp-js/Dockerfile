FROM caddy:2-alpine@sha256:003a97808e781aa9f7e75cc2f43d34d461099031faf5e20faefb9f61126836f0 AS proxy
FROM wholelottahoopla/mcp-js:0.22.0@sha256:d08ea2be9efec96a613ae2ac480b1ff8dfe405e0b3a69b470b599ca1788f72be
COPY --from=proxy /usr/bin/caddy /usr/local/bin/caddy
COPY Caddyfile start.sh /opt/wreview/
EXPOSE 8081
ENTRYPOINT ["/bin/sh", "/opt/wreview/start.sh"]
