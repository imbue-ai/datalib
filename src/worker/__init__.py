"""Long-running worker that drains the `sync_jobs` Dolt-backed queue.

The backend (`frankweiler/backend`) supervises this process the same way
it supervises `dolt sql-server`: spawn on startup, restart on crash,
SIGTERM on shutdown. The worker itself is plain Python so the same
package can also be run standalone for development (`python -m worker`).
"""
