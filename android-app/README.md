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
- `/client/*`, `/auth/device/google`, and app update routes available through the native app endpoint
- when the native endpoint is behind Cloudflare Access, a device certificate enrolled through `sm enroll-device`

## Publish a new APK

Build the app:

```
cd android-app
SM_VERSION_CODE=2 SM_VERSION_NAME=0.1.1 ./gradlew assembleDebug
```

Then publish it to the local Session Manager artifact server:

```
cd ..
VERSION_CODE=2 VERSION_NAME=0.1.1 ./scripts/deploy_android_app.sh
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
- run `sm enroll-device` on the trusted Session Manager host and scan its QR
  code from Settings within the short enrollment window
- the app submits its Android Keystore CSR and public key to the pairing URL
- Session Manager issues an SM mobile Cloudflare Access client certificate
  whose Common Name matches that device key id
- the app stores the signed certificate chain returned by enrollment
- authenticate the native HTTPS origin with Cloudflare Access client-certificate proof
- sign in with Google and exchange the ID token for the SM device bearer token
- request an in-app terminal attach ticket using the Android Keystore device key

Cloudflare client-certificate proof is only the public-edge gate. The app still needs the SM bearer token and route-local attach-ticket proof before shell access is available.

QR enrollment through the phone Camera app is the supported path. The app does
not request camera permission, does not include an in-app scanner, and does not
expose the signed certificate chain in Settings. The server pairing token should
be single-use and expire after about 15 minutes.
