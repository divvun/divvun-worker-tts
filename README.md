# divvun-worker-tts

## Usage

Preferably, download a recent binary from the releases. Building this is a painfully involved process at the moment.

Then:

```bash
divvun-worker-tts path/to/files
```

Your files directory should include:

- `tts.drb` -- this holds your speech synthesizer model
- `text-XXX.drb` -- the text processing model for a given language code (e.g. `olo`)
- `config.toml` -- this describes how text processing hooks up with the synthesizer

## Testing

You can access a web UI for testing on `/`.

`/health` provides a server health check.

## API

See <https://api.giellalt.org/#tts>.

## License

Apache-2.0 OR MIT.