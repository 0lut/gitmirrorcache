import json
import sys

actions = [{
    "Type": "fixed-response",
    "FixedResponseConfig": {
        "StatusCode": "404",
        "ContentType": "text/plain",
        "MessageBody": "preview version not found",
    },
}]
json.dump(actions, open(sys.argv[1], "w"))
