ops = [create_stream(name = "orders")] + [
    create_topic(
        stream = "orders",
        name = name,
        partitions = per_env(dev = 1, staging = 3, prod = 12),
        message_expiry = "7d",
    )
    for name in ["created", "paid", "shipped"]
] + [create_consumer_group(stream = "orders", topic = "paid", name = "billing")]
