# Remote administration of the deployed instance.
#
# Override any variable on the command line, e.g.
#   just host=staging.desmottes.be add-user alice

# --- deployment target -------------------------------------------------------

host     := "cookbook.desmottes.be"
ssh_user := env("USER")

# service account and paths on the server
app_user := "cookbook"
service  := "cookbook"
bin      := "/home/cookbook/bin/cooking-ebook-viewer"
library  := "/home/cookbook/books"
db       := "/home/cookbook/library.db"
auth_db  := "/home/cookbook/auth.db"

# `just` needs a tty for anything that prompts (sudo, password entry) and for
# `journalctl -f`; the remote login shell is fish, so chain with && rather than
# `set -e`, which fish does not have.

# -----------------------------------------------------------------------------

_default:
    @just --list

# Release build; static/ is embedded, so frontend edits need this too
[group('build')]
build:
    cargo build --release

# Rebuild locally, then install the new binary on the server and restart it
[group('build')]
deploy: build
    scp target/release/cooking-ebook-viewer \
        {{ ssh_user }}@{{ host }}:/tmp/cooking-ebook-viewer.new
    @ssh -t {{ ssh_user }}@{{ host }} ' \
        sudo systemctl stop {{ service }} \
        && sudo install -o {{ app_user }} -g {{ app_user }} -m 755 \
               /tmp/cooking-ebook-viewer.new {{ bin }} \
        && rm -f /tmp/cooking-ebook-viewer.new \
        && sudo systemctl start {{ service }} \
        && sleep 1 \
        && systemctl is-active {{ service }} \
        && curl -sS -o /dev/null -w "serving, HTTP %{http_code}\n" \
               http://127.0.0.1:8080/'

# Rebuild library.db from the epubs (~3 min); the service is down meanwhile
[group('service')]
reindex:
    @ssh -t {{ ssh_user }}@{{ host }} ' \
        sudo systemctl stop {{ service }} \
        && sudo -u {{ app_user }} {{ bin }} index \
               --library {{ library }} --db {{ db }} \
        && sudo -u {{ app_user }} {{ bin }} stats --db {{ db }} \
        && sudo systemctl start {{ service }} \
        && sleep 1 \
        && systemctl is-active {{ service }}'

# Follow the service log; pass journalctl flags, e.g. `just logs -n 200`
[group('service')]
logs *flags='-f':
    @ssh -t {{ ssh_user }}@{{ host }} \
        sudo journalctl -u {{ service }} {{ flags }}

# Service status
[group('service')]
status:
    @ssh -t {{ ssh_user }}@{{ host }} systemctl status {{ service }}

# List accounts
[group('users')]
list-users:
    @ssh {{ ssh_user }}@{{ host }} \
        sudo -u {{ app_user }} {{ bin }} user list --auth-db {{ auth_db }}

# Create an account; prompts for the password on the server's tty
[group('users')]
add-user name:
    @ssh -t {{ ssh_user }}@{{ host }} \
        sudo -u {{ app_user }} {{ bin }} user add {{ quote(name) }} --auth-db {{ auth_db }}

# Change an account's password; signs out that account's existing sessions
[group('users')]
passwd name:
    @ssh -t {{ ssh_user }}@{{ host }} \
        sudo -u {{ app_user }} {{ bin }} user passwd {{ quote(name) }} --auth-db {{ auth_db }}

# Delete an account
[group('users')]
[confirm("Delete this account and everything it owns (cook log, favourites, tags, notes)?")]
rm-user name:
    @ssh -t {{ ssh_user }}@{{ host }} \
        sudo -u {{ app_user }} {{ bin }} user rm {{ quote(name) }} --auth-db {{ auth_db }}
