#!/usr/bin/env sh
cd ../www/game-host
export PORT=3001
export BROWSER=NONE
export PORTOFSERVICE=11409
export URL="http://localhost:$PORT?service_url=http%3A%2F%2F127.0.0.1%3A$PORTOFSERVICE"
$(
    while true; do
        sleep 1 && curl $URL && break
    done
    python3 -m webbrowser $URL
) &
yarn start react
