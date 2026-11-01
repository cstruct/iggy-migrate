let Topic =
      { Type =
          { stream : Text
          , name : Text
          , partitions : Natural
          , message_expiry : Optional Text
          , max_size : Optional Text
          }
      , default =
        { partitions = 1, message_expiry = None Text, max_size = None Text }
      }

let TopicUpdate =
      { Type =
          { stream : Text
          , name : Text
          , partitions : Optional Natural
          , message_expiry : Optional Text
          , max_size : Optional Text
          }
      , default =
        { partitions = None Natural, message_expiry = None Text, max_size = None Text }
      }

let Op =
      < CreateStream : { name : Text }
      | DeleteStream : { name : Text }
      | CreateTopic : Topic.Type
      | UpdateTopic : TopicUpdate.Type
      | DeleteTopic : { stream : Text, name : Text }
      | CreateConsumerGroup : { stream : Text, topic : Text, name : Text }
      | DeleteConsumerGroup : { stream : Text, topic : Text, name : Text }
      >

let map =
      \(a : Type) ->
      \(b : Type) ->
      \(f : a -> b) ->
      \(xs : List a) ->
        List/fold a xs (List b) (\(x : a) -> \(acc : List b) -> [ f x ] # acc) ([] : List b)

let concat =
      \(a : Type) ->
      \(xss : List (List a)) ->
        List/fold (List a) xss (List a) (\(xs : List a) -> \(acc : List a) -> xs # acc) ([] : List a)

