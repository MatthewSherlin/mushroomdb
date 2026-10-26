# Graph retrieval example

`graph_retrieval.py` shows the retrieval step of a retrieval-augmented generation app on mushroomdb: text chunks and the entities they mention become nodes and edges, a question finds the nearest chunks by vector, and a graph hop pulls in related chunks the vector missed.

It runs offline with no API keys: entity extraction is a fixed lookup and the embedding is a deterministic hashed bag-of-words. It uses the core Python API, not `langchain-mushroomdb`, because that package needs `langchain-core` from a package index.

Run it with `pip install mushroomdb && python graph_retrieval.py`.
