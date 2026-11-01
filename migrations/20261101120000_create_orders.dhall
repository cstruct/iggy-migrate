let topic =
      \(name : Text) ->
        Op.CreateTopic
          Topic::{
          , stream = "orders"
          , name
          , partitions = merge { dev = 1, staging = 3, prod = 12 } env.kind
          , message_expiry = Some "7d"
          }

in  [ Op.CreateStream { name = "orders" } ]
  # map Text Op topic [ "created", "paid", "shipped" ]
  # [ Op.CreateConsumerGroup
        { stream = "orders", topic = "paid", name = "billing" }
    ]
