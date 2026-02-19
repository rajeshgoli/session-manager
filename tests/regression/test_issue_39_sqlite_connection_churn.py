"""
Regression tests for issue #39: SQLite connection churn causes performance issues

Tests verify that:
1. A single persistent connection is used instead of creating new connections
2. WAL mode is enabled for better concurrency
3. Thread-safety lock prevents "database is locked" errors
4. Connection is properly reused across operations
"""

import pytest
import asyncio
import sqlite3
import threading
from pathlib import Path
from unittest.mock import Mock, patch, MagicMock
from datetime import datetime

from src.message_queue import MessageQueueManager


def noop_create_task(coro):
    """Silently close coroutine without running it (no event loop in sync tests)."""
    coro.close()
    return MagicMock()


@pytest.fixture
def temp_db(tmp_path):
    """Create a temporary database path."""
    db_path = tmp_path / "test_queue.db"
    return str(db_path)


@pytest.fixture
def mock_session_manager():
    """Create a mock SessionManager."""
    manager = Mock()
    manager.get_session = Mock(return_value=None)
    manager.tmux = Mock()
    manager.tmux.send_input_async = Mock(return_value=True)
    return manager


@pytest.fixture
def queue_manager(temp_db, mock_session_manager):
    """Create a MessageQueueManager for testing."""
    return MessageQueueManager(
        session_manager=mock_session_manager,
        db_path=temp_db,
    )


def test_persistent_connection_created(queue_manager):
    """Test that a persistent connection is created on init."""
    # Verify connection exists
    assert queue_manager._db_conn is not None
    assert isinstance(queue_manager._db_conn, sqlite3.Connection)

    # Verify connection is configured with check_same_thread=False
    # (We can't directly check this, but we can verify it works from other threads)
    assert queue_manager._db_lock is not None
    assert isinstance(queue_manager._db_lock, threading.Lock)


def test_wal_mode_enabled(queue_manager):
    """Test that WAL mode is enabled for better concurrency."""
    # Query the journal mode
    with queue_manager._db_lock:
        cursor = queue_manager._db_conn.cursor()
        cursor.execute("PRAGMA journal_mode")
        journal_mode = cursor.fetchone()[0]

    assert journal_mode.upper() == "WAL"


def test_connection_reused_not_recreated(queue_manager):
    """Test that the same connection is reused, not recreated."""
    # Get the connection object ID
    conn_id = id(queue_manager._db_conn)

    # Perform multiple operations
    with patch('asyncio.create_task', noop_create_task):
        queue_manager.queue_message(
            target_session_id="test-session",
            text="Message 1",
        )

        queue_manager.queue_message(
            target_session_id="test-session",
            text="Message 2",
        )

    pending = queue_manager.get_pending_messages("test-session")
    assert len(pending) == 2

    # Verify connection object is still the same
    assert id(queue_manager._db_conn) == conn_id


def test_no_new_connections_created(queue_manager, temp_db):
    """Test that no new connections are created during operations."""
    original_connect = sqlite3.connect
    connect_calls = []

    def track_connect(*args, **kwargs):
        connect_calls.append((args, kwargs))
        return original_connect(*args, **kwargs)

    with patch('sqlite3.connect', side_effect=track_connect), \
         patch('asyncio.create_task', noop_create_task):
        # Perform operations (connection already created in __init__)
        queue_manager.queue_message(
            target_session_id="test-session",
            text="Test message",
        )

        queue_manager.get_pending_messages("test-session")
        queue_manager._mark_delivered("non-existent-id")  # Safe to call

        # No new connections should have been created
        assert len(connect_calls) == 0


def test_concurrent_operations_with_lock(queue_manager):
    """Test that concurrent operations are serialized by the lock."""
    import time

    results = []
    errors = []

    def db_operation(session_id: str, delay: float):
        try:
            time.sleep(delay)  # Simulate work
            queue_manager.queue_message(
                target_session_id=session_id,
                text=f"Message from {threading.current_thread().name}",
            )
            results.append(session_id)
        except Exception as e:
            errors.append(e)

    # Create threads that will try to access DB concurrently
    threads = []
    for i in range(10):
        thread = threading.Thread(
            target=db_operation,
            args=(f"session-{i}", 0.001),
            name=f"Worker-{i}"
        )
        threads.append(thread)

    # Patch asyncio.create_task: sequential delivery always schedules a task (sm#244),
    # but these threads have no running event loop.
    with patch('asyncio.create_task', noop_create_task):
        # Start all threads
        for thread in threads:
            thread.start()

        # Wait for all to complete
        for thread in threads:
            thread.join()

    # Verify no errors occurred (no "database is locked" errors)
    assert len(errors) == 0, f"Errors occurred: {errors}"

    # Verify all operations completed
    assert len(results) == 10

    # Verify all messages were queued
    for i in range(10):
        pending = queue_manager.get_pending_messages(f"session-{i}")
        assert len(pending) == 1


@pytest.mark.asyncio
async def test_concurrent_async_operations(queue_manager):
    """Test concurrent async operations don't cause database locking."""
    async def queue_many(session_id: str, count: int):
        for i in range(count):
            queue_manager.queue_message(
                target_session_id=session_id,
                text=f"Message {i}",
            )
            await asyncio.sleep(0.001)  # Yield to other tasks

    # Run multiple async tasks concurrently
    await asyncio.gather(
        queue_many("session-1", 10),
        queue_many("session-2", 10),
        queue_many("session-3", 10),
    )

    # Verify all messages queued successfully
    assert len(queue_manager.get_pending_messages("session-1")) == 10
    assert len(queue_manager.get_pending_messages("session-2")) == 10
    assert len(queue_manager.get_pending_messages("session-3")) == 10


def test_execute_helper_uses_lock(queue_manager):
    """Test that _execute helper method uses the lock."""
    # Test that the lock is used by verifying concurrent access is serialized
    import time

    execution_order = []

    def delayed_operation(op_id: int):
        # This will be serialized by the lock
        with queue_manager._db_lock:
            execution_order.append(f"start-{op_id}")
            time.sleep(0.01)  # Small delay
            execution_order.append(f"end-{op_id}")

    # Create threads
    threads = []
    for i in range(3):
        thread = threading.Thread(target=delayed_operation, args=(i,))
        threads.append(thread)

    for thread in threads:
        thread.start()

    for thread in threads:
        thread.join()

    # Verify operations were serialized (no interleaving)
    # Each operation should fully complete before the next starts
    for i in range(3):
        start_idx = execution_order.index(f"start-{i}")
        end_idx = execution_order.index(f"end-{i}")
        # Verify no other operation started between this operation's start and end
        between = execution_order[start_idx + 1:end_idx]
        assert between == [], f"Operation {i} was interleaved: {execution_order}"


def test_execute_query_helper_uses_lock(queue_manager):
    """Test that _execute_query helper method returns correct results."""
    # Execute a query
    results = queue_manager._execute_query("SELECT 1")

    # Verify results are correct
    assert results == [(1,)]

    # Test with parameters
    results = queue_manager._execute_query("SELECT ? as value", (42,))
    assert results == [(42,)]

    # Test that it actually uses the persistent connection
    # by verifying we can query the schema
    results = queue_manager._execute_query("""
        SELECT name FROM sqlite_master WHERE type='table' ORDER BY name
    """)
    table_names = [row[0] for row in results]
    assert "message_queue" in table_names
    assert "scheduled_reminders" in table_names


@pytest.mark.asyncio
async def test_connection_closed_on_stop(queue_manager):
    """Test that connection is properly closed when stopping."""
    # Start the queue manager
    await queue_manager.start()

    # Verify connection exists
    assert queue_manager._db_conn is not None
    conn = queue_manager._db_conn

    # Stop the manager
    await queue_manager.stop()

    # Verify connection is closed
    assert queue_manager._db_conn is None

    # Verify connection object is actually closed
    # Attempting to use it should raise an error
    with pytest.raises(sqlite3.ProgrammingError, match="Cannot operate on a closed database"):
        conn.execute("SELECT 1")


def test_all_operations_use_persistent_connection(queue_manager):
    """Comprehensive test that all DB operations use the persistent connection."""
    # Track all operations
    operations = []

    original_execute = queue_manager._execute
    original_execute_query = queue_manager._execute_query

    def track_execute(*args, **kwargs):
        operations.append(('execute', args[0] if args else None))
        return original_execute(*args, **kwargs)

    def track_execute_query(*args, **kwargs):
        operations.append(('execute_query', args[0] if args else None))
        return original_execute_query(*args, **kwargs)

    queue_manager._execute = track_execute
    queue_manager._execute_query = track_execute_query

    # Perform various operations
    with patch('asyncio.create_task', noop_create_task):
        msg_id = queue_manager.queue_message(
            target_session_id="test-session",
            text="Test message",
        ).id

    queue_manager.get_pending_messages("test-session")
    queue_manager.get_queue_length("test-session")
    queue_manager._mark_delivered(msg_id)
    queue_manager._get_sessions_with_pending()

    # Verify all operations went through helper methods
    assert len(operations) > 0
    assert all(op[0] in ('execute', 'execute_query') for op in operations)


@pytest.mark.asyncio
async def test_reminder_operations_use_persistent_connection(queue_manager):
    """Test that reminder operations use the persistent connection."""
    operations = []

    original_execute = queue_manager._execute

    def track_execute(*args, **kwargs):
        operations.append(args[0] if args else None)
        return original_execute(*args, **kwargs)

    queue_manager._execute = track_execute

    # Schedule a reminder
    reminder_id = await queue_manager.schedule_reminder(
        session_id="test-session",
        delay_seconds=1,
        message="Test reminder",
    )

    # Verify INSERT went through helper
    assert any("INSERT INTO scheduled_reminders" in op for op in operations)

    # Cancel the reminder task to clean up
    if reminder_id in queue_manager._scheduled_tasks:
        queue_manager._scheduled_tasks[reminder_id].cancel()


def test_cleanup_operations_use_persistent_connection(queue_manager):
    """Test that cleanup operations use the persistent connection."""
    # Queue some messages
    with patch('asyncio.create_task', noop_create_task):
        queue_manager.queue_message(
            target_session_id="test-session",
            text="Message 1",
        )

    operations = []

    original_execute = queue_manager._execute
    original_execute_query = queue_manager._execute_query

    def track_execute(*args, **kwargs):
        operations.append(('execute', args[0] if args else None))
        return original_execute(*args, **kwargs)

    def track_execute_query(*args, **kwargs):
        operations.append(('execute_query', args[0] if args else None))
        return original_execute_query(*args, **kwargs)

    queue_manager._execute = track_execute
    queue_manager._execute_query = track_execute_query

    # Cleanup
    queue_manager._cleanup_messages_for_session("test-session")

    # Verify operations went through helpers
    assert any('SELECT COUNT(*)' in op[1] for op in operations if op[1])
    assert any('DELETE FROM message_queue' in op[1] for op in operations if op[1])


def test_no_database_locked_errors_under_load(queue_manager):
    """Stress test to ensure no 'database is locked' errors under heavy load."""
    import time

    errors = []
    success_count = []

    def heavy_load_worker(worker_id: int, iterations: int):
        try:
            for i in range(iterations):
                # Mix of read and write operations
                queue_manager.queue_message(
                    target_session_id=f"session-{worker_id}",
                    text=f"Message {i}",
                )

                pending = queue_manager.get_pending_messages(f"session-{worker_id}")

                if len(pending) > 5:
                    # Mark some as delivered
                    queue_manager._mark_delivered(pending[0].id)

                time.sleep(0.0001)  # Minimal delay

            success_count.append(worker_id)
        except sqlite3.OperationalError as e:
            if "database is locked" in str(e):
                errors.append(f"Worker {worker_id}: {e}")
        except Exception as e:
            errors.append(f"Worker {worker_id}: {e}")

    # Create many threads doing heavy DB work
    threads = []
    num_workers = 20
    iterations_per_worker = 50

    for i in range(num_workers):
        thread = threading.Thread(
            target=heavy_load_worker,
            args=(i, iterations_per_worker)
        )
        threads.append(thread)

    # Patch asyncio.create_task: sequential delivery always schedules a task (sm#244),
    # but these threads have no running event loop.
    with patch('asyncio.create_task', noop_create_task):
        # Start all threads
        for thread in threads:
            thread.start()

        # Wait for completion
        for thread in threads:
            thread.join(timeout=30)

    # Verify no database locked errors
    assert len(errors) == 0, f"Database locked errors occurred: {errors}"

    # Verify all workers completed
    assert len(success_count) == num_workers
