from datetime import datetime, timedelta, timezone
import sys

last_accessed_at = datetime.fromisoformat(sys.argv[1].replace("Z", "+00:00"))
older_than_days = int(sys.argv[2])
cutoff = datetime.now(timezone.utc) - timedelta(days=older_than_days)
sys.exit(0 if last_accessed_at < cutoff else 1)
