# Session Manager Android App

Native Android client for Session Manager.

## Local config

Create `local.defaults.properties` next to this README with:

```
SM_DEFAULT_SERVER_URL=https://your-sm-host
SM_GOOGLE_SERVER_CLIENT_ID=your-web-client-id.apps.googleusercontent.com
```

The real file is gitignored.

## Build

Use Android Studio or a local JDK 17 + Android SDK.

```
cd android-app
./gradlew assembleDebug
```

The app expects:
- external Session Manager origin protected by Google auth
- `/auth/device/google` enabled on the server
- `/apps/session-manager-android/meta.json` and `/apps/session-manager-android/latest.apk` available on the server
- Termux installed for direct tmux attach
- SSH attach exposed through a tunnel such as Cloudflare Access SSH
- SSH origin configured for public-key auth only; password auth should remain disabled

## Publish a new APK

Build the app:

```
cd android-app
./gradlew assembleDebug
```

Then publish it to the local Session Manager artifact server:

```
cd ..
VERSION_NAME=0.1.0 ./scripts/deploy_android_app.sh
```

By default the deploy script uploads:
- app: `session-manager-android`
- server: `http://127.0.0.1:8420`
- APK: `android-app/app/build/outputs/apk/debug/app-debug.apk`

You can override those with `APP_NAME`, `SERVER_URL`, `VERSION_CODE`, `VERSION_NAME`, or by passing a different APK path as the first argument.

The server stores:
- `latest.apk` for convenience
- immutable hashed APKs for cache-safe installs
- `meta.json` for in-app update checks

## Attach security model

The intended Android attach path is:
- sign in to the HTTPS origin with Google
- authenticate the SSH tunnel with Cloudflare Access
- connect from Termux using a previously authorized SSH key

The app does not assume password-based SSH access.
