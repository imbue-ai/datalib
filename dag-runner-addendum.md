- i'm not quite sure about the terminology of "node". it seems to imply data, but it's actually action. it could use some more thoughts. what's a standard terminology in other dag runners?

- i'm not sure about the storage layout. it could use more thoughts. just do whatever results in the least change for now.

- i envision that after we have generalized into the dag runner, the config format will completely change. the current "data sources" will exist, but for most of them, only their initial download step will be a distinct "node" type. all the subsequence processing steps will be instances of the same "node" type. there are some data sources that have their own unique post-processing steps though, and those can still be distinct node types.

- the steps after download are mostly shared between different data sources today. this means that they will have an input that is basically "the output of all download steps". we can support some kind of "wildcard" input, as long as that fits the current design. make your own choice during prototyping and tell me more about the trade-offs afterwards.
