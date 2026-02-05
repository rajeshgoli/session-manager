"""Background monitoring service for child sessions with --wait flag."""

import asyncio
import logging
from datetime import datetime, timedelta
from typing import Optional, Dict, Set
from pathlib import Path
import json

from .models import DeliveryResult

logger = logging.getLogger(__name__)


class ChildMonitor:
    """Monitors child sessions and notifies parents when complete or idle."""

    def __init__(self, session_manager):
        """
        Initialize child monitor.

        Args:
            session_manager: SessionManager instance
        """
        self.session_manager = session_manager
        self.monitored_children: Dict[str, dict] = {}  # child_id -> {parent_id, wait_seconds, started_at}
        self.monitoring_tasks: Dict[str, asyncio.Task] = {}  # child_id -> monitoring task
        self._running = False

    async def start(self):
        """Start the monitoring service."""
        self._running = True
        logger.info("Child monitor started")

    async def stop(self):
        """Stop the monitoring service and cancel all monitoring tasks."""
        self._running = False
        # Cancel all monitoring tasks
        for task in self.monitoring_tasks.values():
            task.cancel()
        self.monitoring_tasks.clear()
        logger.info("Child monitor stopped")

    def register_child(self, child_session_id: str, parent_session_id: str, wait_seconds: int):
        """
        Register a child session for monitoring.

        Args:
            child_session_id: Child session ID to monitor
            parent_session_id: Parent session ID to notify
            wait_seconds: Idle timeout in seconds
        """
        self.monitored_children[child_session_id] = {
            "parent_id": parent_session_id,
            "wait_seconds": wait_seconds,
            "started_at": datetime.now(),
        }

        # Start monitoring task
        task = asyncio.create_task(self._monitor_child(child_session_id))
        self.monitoring_tasks[child_session_id] = task

        logger.info(f"Registered child {child_session_id} for monitoring (parent={parent_session_id}, wait={wait_seconds}s)")

    async def _monitor_child(self, child_session_id: str):
        """
        Monitor a child session for completion or idle timeout.

        Args:
            child_session_id: Child session ID to monitor
        """
        try:
            monitor_info = self.monitored_children.get(child_session_id)
            if not monitor_info:
                return

            parent_session_id = monitor_info["parent_id"]
            wait_seconds = monitor_info["wait_seconds"]

            logger.info(f"Started monitoring child {child_session_id}")

            # Poll child session status
            while self._running:
                child_session = self.session_manager.get_session(child_session_id)
                if not child_session:
                    logger.warning(f"Child session {child_session_id} not found, stopping monitoring")
                    break

                # Check if child has exited (tmux only)
                if getattr(child_session, "provider", "claude") != "codex":
                    if not self.session_manager.tmux.session_exists(child_session.tmux_session):
                        logger.info(f"Child {child_session_id} tmux session exited")
                        await self._notify_parent_completion(
                            parent_session_id,
                            child_session_id,
                            "Session exited"
                        )
                        break

                # Check for idle timeout
                if child_session.last_tool_call:
                    idle_time = (datetime.now() - child_session.last_tool_call).total_seconds()
                elif getattr(child_session, "provider", "claude") == "codex":
                    if self.session_manager.is_codex_turn_active(child_session_id):
                        await asyncio.sleep(5)
                        continue
                    idle_time = (datetime.now() - child_session.last_activity).total_seconds()
                else:
                    # No tool call yet, check since spawned_at
                    idle_time = (datetime.now() - (child_session.spawned_at or child_session.created_at)).total_seconds()

                if idle_time >= wait_seconds:
                    logger.info(f"Child {child_session_id} idle for {idle_time}s (threshold: {wait_seconds}s)")
                    # Extract completion message from recent output
                    completion_msg = await self._extract_completion_message(child_session_id)
                    await self._notify_parent_completion(
                        parent_session_id,
                        child_session_id,
                        completion_msg or f"Idle for {int(idle_time)}s"
                    )
                    break

                # Check for completion status
                from src.models import CompletionStatus
                if child_session.completion_status == CompletionStatus.COMPLETED:
                    logger.info(f"Child {child_session_id} marked as completed")
                    await self._notify_parent_completion(
                        parent_session_id,
                        child_session_id,
                        child_session.completion_message or "Completed"
                    )
                    break

                # Poll every 5 seconds
                await asyncio.sleep(5)

        except asyncio.CancelledError:
            logger.info(f"Monitoring cancelled for child {child_session_id}")
        except Exception as e:
            logger.error(f"Error monitoring child {child_session_id}: {e}")
        finally:
            # Clean up
            self.monitored_children.pop(child_session_id, None)
            self.monitoring_tasks.pop(child_session_id, None)

    async def _extract_completion_message(self, child_session_id: str) -> Optional[str]:
        """
        Extract completion message from child's recent transcript or output.

        Args:
            child_session_id: Child session ID

        Returns:
            Completion message or None
        """
        child_session = self.session_manager.get_session(child_session_id)
        if not child_session:
            return None

        if getattr(child_session, "provider", "claude") == "codex":
            store = getattr(self.session_manager, "hook_output_store", None)
            if store:
                last = store.get(child_session_id)
                if last:
                    first_sentence = last.split('.')[0]
                    if len(first_sentence) > 100:
                        first_sentence = first_sentence[:100] + "..."
                    return first_sentence

        # Try to read from transcript if available
        if child_session.transcript_path:
            try:
                transcript_file = Path(child_session.transcript_path)
                if transcript_file.exists():
                    # Read last few lines
                    lines = transcript_file.read_text().strip().split('\n')
                    # Get last assistant message
                    for line in reversed(lines[-20:]):  # Check last 20 lines
                        try:
                            entry = json.loads(line)
                            if entry.get("type") == "assistant":
                                message = entry.get("message", {})
                                content = message.get("content", [])
                                texts = []
                                for item in content:
                                    if isinstance(item, dict) and item.get("type") == "text":
                                        text = item.get("text", "")
                                        # Extract first sentence as summary
                                        if text:
                                            first_sentence = text.split('.')[0]
                                            if len(first_sentence) > 100:
                                                first_sentence = first_sentence[:100] + "..."
                                            return first_sentence
                        except json.JSONDecodeError:
                            continue
            except Exception as e:
                logger.error(f"Error reading transcript: {e}")

        # Fallback: capture recent tmux output
        output = self.session_manager.capture_output(child_session_id, lines=10)
        if output:
            # Extract first non-empty line
            for line in output.strip().split('\n'):
                line = line.strip()
                if line and len(line) > 10:
                    if len(line) > 100:
                        line = line[:100] + "..."
                    return line

        return None

    async def _notify_parent_completion(
        self,
        parent_session_id: str,
        child_session_id: str,
        completion_message: str,
    ):
        """
        Notify parent session that child has completed.

        Args:
            parent_session_id: Parent session ID
            child_session_id: Child session ID
            completion_message: Completion summary message
        """
        child_session = self.session_manager.get_session(child_session_id)
        if not child_session:
            return

        child_name = child_session.friendly_name or child_session.name or child_session_id

        # Format notification message
        notification = f"Child {child_name} ({child_session_id[:8]}) completed: {completion_message}"

        # Send to parent's input
        result = await self.session_manager.send_input(
            parent_session_id,
            notification,
            sender_session_id=child_session_id
        )

        if result != DeliveryResult.FAILED:
            logger.info(f"Sent completion notification to parent {parent_session_id}: {notification} (result={result.value})")
            # Mark child as completed
            if child_session:
                from src.models import CompletionStatus
                child_session.completion_status = CompletionStatus.COMPLETED
                child_session.completion_message = completion_message
                child_session.completed_at = datetime.now()
                self.session_manager._save_state()
        else:
            logger.error(f"Failed to send completion notification to parent {parent_session_id}")

    def unregister_child(self, child_session_id: str):
        """
        Stop monitoring a child session.

        Args:
            child_session_id: Child session ID
        """
        # Cancel monitoring task if exists
        task = self.monitoring_tasks.get(child_session_id)
        if task:
            task.cancel()

        self.monitored_children.pop(child_session_id, None)
        self.monitoring_tasks.pop(child_session_id, None)

        logger.info(f"Unregistered child {child_session_id} from monitoring")
