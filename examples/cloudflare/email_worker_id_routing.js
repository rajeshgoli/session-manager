function normalizeEmailAddress(value) {
  const raw = String(value || "").trim().toLowerCase();
  const match = raw.match(/<([^>]+)>/);
  return (match ? match[1] : raw).trim();
}

async function readRawEmail(message) {
  const chunks = [];
  const reader = message.raw.getReader();

  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
  }

  const total = chunks.reduce((count, chunk) => count + chunk.length, 0);
  const merged = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.length;
  }

  return new TextDecoder().decode(merged);
}

export default {
  async email(message, env, ctx) {
    const allowList = String(env.ALLOWED_SENDERS || "")
      .split(",")
      .map((value) => normalizeEmailAddress(value))
      .filter(Boolean);

    const from = normalizeEmailAddress(message.from);
    if (!allowList.includes(from)) {
      message.setReject(`Address not allowed: ${from}`);
      return;
    }

    const to = normalizeEmailAddress(message.to);
    const localPart = to.split("@")[0] || "";
    if (!/^[a-z0-9]{8}$/.test(localPart)) {
      message.setReject(`Invalid Session Manager recipient: ${to}`);
      return;
    }

    const rawEmail = await readRawEmail(message);
    if (!rawEmail) {
      message.setReject("Empty raw email");
      return;
    }

    const response = await fetch(env.SM_WEBHOOK_URL, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-email-worker-secret": env.EMAIL_WORKER_SECRET,
        "x-email-session-id": localPart,
      },
      body: JSON.stringify({
        raw_email: rawEmail,
        from_address: from,
      }),
    });

    if (!response.ok) {
      throw new Error(`Webhook failed: ${response.status}`);
    }
  },
};
