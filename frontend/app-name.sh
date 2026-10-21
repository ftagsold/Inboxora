#!/bin/sh
# Fills the app name from VITE_APP_NAME into the served index.html and manifest.json at
# container start-up, so renaming the app needs only an environment variable, not a rebuild.
# Runs automatically via /docker-entrypoint.d/. The built files are kept as *.template, so a
# restart with a different name renders again from the originals.
set -eu

root=${APP_NAME_HTML_ROOT:-/usr/share/nginx/html}  # overridable for tests
placeholder=__INBOXORA_APP_NAME__
name=$(printf '%s' "${VITE_APP_NAME:-Inboxora}" | tr -d '\r\n')
[ -n "$name" ] || name=Inboxora

html=$(printf '%s' "$name" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g' -e "s/'/\&#39;/g")
json=$(printf '%s' "$name" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
# Escape the sed replacement itself: backslash, ampersand and the | delimiter.
sed_replacement() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

render() {
  file="$root/$1"
  [ -f "$file.template" ] || cp "$file" "$file.template"
  sed "s|$placeholder|$(sed_replacement "$2")|g" "$file.template" > "$file.tmp"
  mv "$file.tmp" "$file"
}

render index.html "$html"
render manifest.json "$json"
echo "app-name: serving as \"$name\""
