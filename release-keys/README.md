# Release keys

`client-update.pub` is the Ed25519 public key that is compiled into the client
and the coordinator. A client installs an update only if its signature verifies
against this key, so only whoever holds the matching private key can ship
updates to devices. The client runs as LocalSystem, which is why this check
matters.

The file is absent until the repository owner generates the key pair. Until it
is committed, builds have no update key and refuse every client update.

## Create the key pair (once, by the owner)

1. Generate the pair and write the public half into the repository:

   ```
   cargo run -p release-tool -- keygen --public-out release-keys/client-update.pub
   ```

2. The command prints the private key once. Store it as the GitHub Actions
   repository secret `SC_UPDATE_SIGNING_KEY` and nowhere else. The tool never
   writes it to a file; do not put it in the repository.
3. Commit `release-keys/client-update.pub`.

CI can check that the secret still matches the committed key:
`release-tool public-key` (with `SC_UPDATE_SIGNING_KEY` set) prints the public
key for comparison with the file.

## Rotate the key

Clients trust only the public key they were built with, so a new key cannot
simply replace the old one:

1. Generate a new pair and commit the new `client-update.pub`.
2. Build a release containing the NEW public key but sign it with the OLD
   private key. Devices accept it because it verifies against the old key they
   carry.
3. Once the devices run that release, set the secret `SC_UPDATE_SIGNING_KEY` to
   the new private key; later releases are signed with the new key.

Replacing the secret before step 2 has reached the devices strands them: they
would reject every update signed with the new key.
