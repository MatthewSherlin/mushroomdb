# Permissioned assistant example

`permissioned_assistant.py` shows the retrieval step of an AI assistant whose users may see different documents. Two people ask the same question of one graph and get different context, because every read goes through a handle scoped to the asker's role. Vector search and the walk along derived edges both obey that scope, so the app has no filtering step to forget.

It runs offline with no API keys: the embedding is a deterministic hashed bag-of-words vector, and the printed context is what a prompt would carry.

Run it with `pip install mushroomdb && python permissioned_assistant.py`.
