<div align="center">
<img width="1200" height="475" alt="GHBanner" src="https://github.com/user-attachments/assets/0aa67016-6eaf-458a-adb2-6e31a0763ed6" />
</div>

# Run and deploy your AI Studio app

This contains everything you need to run your app locally.

View your app in AI Studio: https://ai.studio/apps/d52b720d-a5a1-42fd-9dab-c6823f328c98

## Run Locally

**Prerequisites:**  Node.js


1. Install dependencies:
   `npm install`
2. Set the `GEMINI_API_KEY` in [.env.local](.env.local) to your Gemini API key
3. Run the app:
   `npm run dev`

## Deploy this dashboard with session-manager

Build output is consumed directly by `session-manager` from `web/sm-watch/dist`.

1. Build:
   `cd web/sm-watch && npm install && npm run build`
2. Start the session manager server.
3. Open: `http://localhost:8420/watch`

If the build does not exist, the server returns a 503 message with the same build command.
