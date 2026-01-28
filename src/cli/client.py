"""HTTP client for Session Manager API."""

import os
import sys
from typing import Optional
import urllib.request
import urllib.error
import json

# Default API endpoint
DEFAULT_API_URL = "http://127.0.0.1:8420"
API_TIMEOUT = 2  # seconds


class SessionManagerClient:
    """Client for Session Manager API."""

    def __init__(self, api_url: Optional[str] = None):
        """
        Initialize client.

        Args:
            api_url: Base URL for API (default: http://127.0.0.1:8420)
        """
        self.api_url = api_url or os.environ.get("SM_API_URL", DEFAULT_API_URL)
        self.session_id = os.environ.get("CLAUDE_SESSION_MANAGER_ID")

    def _request(self, method: str, path: str, data: Optional[dict] = None, timeout: Optional[int] = None) -> tuple[Optional[dict], bool, bool]:
        """
        Make an HTTP request.

        Args:
            method: HTTP method (GET, POST, PUT, PATCH, DELETE)
            path: API path
            data: Optional JSON data
            timeout: Optional timeout in seconds (default: API_TIMEOUT)

        Returns:
            Tuple of (response_data, success, unavailable)
            - success=True, unavailable=False: Request succeeded
            - success=False, unavailable=True: Connection error (session manager unavailable)
            - success=False, unavailable=False: API error (4xx, 5xx response)
        """
        url = f"{self.api_url}{path}"
        request_timeout = timeout if timeout is not None else API_TIMEOUT

        try:
            headers = {"Content-Type": "application/json"}
            body = json.dumps(data).encode() if data else None

            req = urllib.request.Request(url, data=body, headers=headers, method=method)
            with urllib.request.urlopen(req, timeout=request_timeout) as response:
                if response.status in (200, 201):
                    return json.loads(response.read().decode()), True, False
                # API responded but with error status
                return None, False, False

        except urllib.error.URLError as e:
            # Connection refused, timeout, etc. - session manager unavailable
            return None, False, True
        except Exception as e:
            # Other errors - treat as unavailable
            return None, False, True

    def get_session(self, session_id: str) -> Optional[dict]:
        """Get session details."""
        data, success, _ = self._request("GET", f"/sessions/{session_id}")
        return data if success else None

    def list_sessions(self) -> Optional[list]:
        """List all sessions."""
        data, success, _ = self._request("GET", "/sessions")
        if success and data:
            return data.get("sessions", [])
        return None

    def update_friendly_name(self, session_id: str, friendly_name: str) -> tuple[bool, bool]:
        """
        Update session friendly name.

        Returns:
            Tuple of (success, unavailable)
        """
        data, success, unavailable = self._request(
            "PATCH",
            f"/sessions/{session_id}",
            {"friendly_name": friendly_name}
        )
        return success, unavailable

    def update_task(self, session_id: str, task: str) -> tuple[bool, bool]:
        """
        Update session current task.

        Returns:
            Tuple of (success, unavailable)
        """
        data, success, unavailable = self._request(
            "PUT",
            f"/sessions/{session_id}/task",
            {"task": task}
        )
        return success, unavailable

    def get_summary(self, session_id: str, lines: int = 100) -> Optional[str]:
        """Get AI-generated summary of session activity."""
        # Summary generation can take up to 60s, use longer timeout
        data, success, _ = self._request("GET", f"/sessions/{session_id}/summary?lines={lines}", timeout=65)
        if success and data:
            return data.get("summary")
        return None

    def register_subagent_start(self, session_id: str, agent_id: str, agent_type: str, transcript_path: Optional[str] = None) -> tuple[bool, bool]:
        """
        Register a subagent start.

        Returns:
            Tuple of (success, unavailable)
        """
        data, success, unavailable = self._request(
            "POST",
            f"/sessions/{session_id}/subagents",
            {
                "agent_id": agent_id,
                "agent_type": agent_type,
                "transcript_path": transcript_path,
            }
        )
        return success, unavailable

    def register_subagent_stop(self, session_id: str, agent_id: str, summary: Optional[str] = None) -> tuple[bool, bool]:
        """
        Register a subagent stop.

        Returns:
            Tuple of (success, unavailable)
        """
        data, success, unavailable = self._request(
            "POST",
            f"/sessions/{session_id}/subagents/{agent_id}/stop",
            {"summary": summary}
        )
        return success, unavailable

    def list_subagents(self, session_id: str) -> Optional[list]:
        """List all subagents for a session."""
        data, success, _ = self._request("GET", f"/sessions/{session_id}/subagents")
        if success and data:
            return data.get("subagents", [])
        return None

    def send_input(
        self,
        session_id: str,
        text: str,
        sender_session_id: Optional[str] = None,
        delivery_mode: str = "sequential",
        from_sm_send: bool = False,
    ) -> tuple[bool, bool]:
        """
        Send text input to a session.

        Args:
            session_id: Target session ID
            text: Text to send to the session's Claude input
            sender_session_id: Optional sender session ID (for metadata)
            delivery_mode: Delivery mode (sequential, important, urgent)
            from_sm_send: True if called from sm send command (triggers notification)

        Returns:
            Tuple of (success, unavailable)
        """
        payload = {"text": text, "delivery_mode": delivery_mode, "from_sm_send": from_sm_send}
        if sender_session_id:
            payload["sender_session_id"] = sender_session_id

        data, success, unavailable = self._request(
            "POST",
            f"/sessions/{session_id}/input",
            payload
        )
        return success, unavailable

    def spawn_child(
        self,
        parent_session_id: str,
        prompt: str,
        name: Optional[str] = None,
        wait: Optional[int] = None,
        model: Optional[str] = None,
        working_dir: Optional[str] = None,
    ) -> Optional[dict]:
        """
        Spawn a child agent session.

        Args:
            parent_session_id: Parent session ID
            prompt: Initial prompt for the child agent
            name: Friendly name for the child session
            wait: Monitor child and notify when complete or idle for N seconds
            model: Model override (opus, sonnet, haiku)
            working_dir: Working directory override

        Returns:
            Dict with session info or None if unavailable
        """
        payload = {
            "parent_session_id": parent_session_id,
            "prompt": prompt,
        }
        if name:
            payload["name"] = name
        if wait is not None:
            payload["wait"] = wait
        if model:
            payload["model"] = model
        if working_dir:
            payload["working_dir"] = working_dir

        data, success, unavailable = self._request("POST", "/sessions/spawn", payload, timeout=10)
        if unavailable:
            return None
        return data

    def list_children(
        self,
        parent_session_id: str,
        recursive: bool = False,
        status_filter: Optional[str] = None,
    ) -> Optional[dict]:
        """
        List child sessions.

        Args:
            parent_session_id: Parent session ID
            recursive: Include grandchildren
            status_filter: Filter by status (running, completed, error, all)

        Returns:
            Dict with children list or None if unavailable
        """
        path = f"/sessions/{parent_session_id}/children"
        params = []
        if recursive:
            params.append("recursive=true")
        if status_filter:
            params.append(f"status={status_filter}")
        if params:
            path += "?" + "&".join(params)

        data, success, unavailable = self._request("GET", path)
        if unavailable:
            return None
        return data if success else {"children": []}

    def kill_session(
        self,
        requester_session_id: Optional[str],
        target_session_id: str,
    ) -> Optional[dict]:
        """
        Kill a session (with parent-child ownership check).

        Args:
            requester_session_id: Requesting session ID (must be parent)
            target_session_id: Target session ID to kill

        Returns:
            Dict with result or None if unavailable
        """
        payload = {}
        if requester_session_id:
            payload["requester_session_id"] = requester_session_id

        data, success, unavailable = self._request(
            "POST",
            f"/sessions/{target_session_id}/kill",
            payload
        )
        if unavailable:
            return None
        return data
