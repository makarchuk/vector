package metadata

base: components: sinks: aws_cloudwatch_logs: configuration: {
	acknowledgements: {
		description: """
			Controls how acknowledgements are handled for this sink.

			See [End-to-end Acknowledgements][e2e_acks] for more information on how Vector handles event acknowledgement.

			[e2e_acks]: https://vector.dev/docs/about/under-the-hood/architecture/end-to-end-acknowledgements/
			"""
		required: false
		type: object: options: enabled: {
			description: """
				Whether or not end-to-end acknowledgements are enabled.

				When enabled for a sink, any source connected to that sink, where the source supports
				end-to-end acknowledgements as well, will wait for events to be acknowledged by the sink
				before acknowledging them at the source.

				Enabling or disabling acknowledgements at the sink level takes precedence over any global
				[`acknowledgements`][global_acks] configuration.

				[global_acks]: https://vector.dev/docs/reference/configuration/global-options/#acknowledgements
				"""
			required: false
			type: bool: {}
		}
	}
	assume_role: {
		description: """
			The ARN of an [IAM role][iam_role] to assume at startup.

			[iam_role]: https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles.html
			"""
		required: false
		type: string: syntax: "literal"
	}
	auth: {
		description: "Configuration of the authentication strategy for interacting with AWS services."
		required:    false
		type: object: options: {
			access_key_id: {
				description: "The AWS access key ID."
				required:    true
				type: string: syntax: "literal"
			}
			assume_role: {
				description: "The ARN of the role to assume."
				required:    true
				type: string: syntax: "literal"
			}
			credentials_file: {
				description: "Path to the credentials file."
				required:    true
				type: string: syntax: "literal"
			}
			load_timeout_secs: {
				description: "Timeout for successfully loading any credentials, in seconds."
				required:    false
				type: uint: {}
			}
			profile: {
				description: "The credentials profile to use."
				required:    false
				type: string: syntax: "literal"
			}
			region: {
				description: """
					The AWS region to send STS requests to.

					If not set, this will default to the configured region
					for the service itself.
					"""
				required: false
				type: string: syntax: "literal"
			}
			secret_access_key: {
				description: "The AWS secret access key."
				required:    true
				type: string: syntax: "literal"
			}
		}
	}
	batch: {
		description: "Event batching behavior."
		required:    false
		type: object: options: {
			max_bytes: {
				description: """
					The maximum size of a batch that will be processed by a sink.

					This is based on the uncompressed size of the batched events, before they are
					serialized / compressed.
					"""
				required: false
				type: uint: {}
			}
			max_events: {
				description: "The maximum size of a batch, in events, before it is flushed."
				required:    false
				type: uint: {}
			}
			timeout_secs: {
				description: "The maximum age of a batch, in seconds, before it is flushed."
				required:    false
				type: float: {}
			}
		}
	}
	compression: {
		description: """
			Compression configuration.

			All compression algorithms use the default compression level unless otherwise specified.
			"""
		required: false
		type: string: {
			default: "none"
			enum: {
				gzip: """
					[Gzip][gzip] compression.

					[gzip]: https://www.gzip.org/
					"""
				none: "No compression."
				zlib: """
					[Zlib]][zlib] compression.

					[zlib]: https://zlib.net/
					"""
			}
		}
	}
	create_missing_group: {
		description: """
			Dynamically create a [log group][log_group] if it does not already exist.

			This will ignore `create_missing_stream` directly after creating the group and will create
			the first stream.

			[log_group]: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/Working-with-log-groups-and-streams.html
			"""
		required: false
		type: bool: {}
	}
	create_missing_stream: {
		description: """
			Dynamically create a [log stream][log_stream] if it does not already exist.

			[log_stream]: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/Working-with-log-groups-and-streams.html
			"""
		required: false
		type: bool: {}
	}
	encoding: {
		description: "Encoding configuration."
		required:    true
		type: object: options: {
			avro: {
				description:   "Apache Avro-specific encoder options."
				relevant_when: "codec = \"avro\""
				required:      true
				type: object: options: schema: {
					description: "The Avro schema."
					required:    true
					type: string: syntax: "literal"
				}
			}
			codec: {
				required: true
				type: string: enum: {
					avro: """
						Encodes an event as an [Apache Avro][apache_avro] message.

						[apache_avro]: https://avro.apache.org/
						"""
					gelf: """
						Encodes an event as a [GELF][gelf] message.

						[gelf]: https://docs.graylog.org/docs/gelf
						"""
					json: """
						Encodes an event as [JSON][json].

						[json]: https://www.json.org/
						"""
					logfmt: """
						Encodes an event as a [logfmt][logfmt] message.

						[logfmt]: https://brandur.org/logfmt
						"""
					native: """
						Encodes an event in Vector’s [native Protocol Buffers format][vector_native_protobuf]([EXPERIMENTAL][experimental]).

						[vector_native_protobuf]: https://github.com/vectordotdev/vector/blob/master/lib/vector-core/proto/event.proto
						[experimental]: https://vector.dev/highlights/2022-03-31-native-event-codecs
						"""
					native_json: """
						Encodes an event in Vector’s [native JSON format][vector_native_json]([EXPERIMENTAL][experimental]).

						[vector_native_json]: https://github.com/vectordotdev/vector/blob/master/lib/codecs/tests/data/native_encoding/schema.cue
						[experimental]: https://vector.dev/highlights/2022-03-31-native-event-codecs
						"""
					raw_message: """
						No encoding.

						This "encoding" simply uses the `message` field of a log event.

						Users should take care if they're modifying their log events (such as by using a `remap`
						transform, etc) and removing the message field while doing additional parsing on it, as this
						could lead to the encoding emitting empty strings for the given event.
						"""
					text: """
						Plaintext encoding.

						This "encoding" simply uses the `message` field of a log event.

						Users should take care if they're modifying their log events (such as by using a `remap`
						transform, etc) and removing the message field while doing additional parsing on it, as this
						could lead to the encoding emitting empty strings for the given event.
						"""
				}
			}
			except_fields: {
				description: "List of fields that will be excluded from the encoded event."
				required:    false
				type: array: items: type: string: syntax: "literal"
			}
			only_fields: {
				description: "List of fields that will be included in the encoded event."
				required:    false
				type: array: items: type: string: syntax: "literal"
			}
			timestamp_format: {
				description: "Format used for timestamp fields."
				required:    false
				type: string: enum: {
					rfc3339: "Represent the timestamp as a RFC 3339 timestamp."
					unix:    "Represent the timestamp as a Unix timestamp."
				}
			}
		}
	}
	endpoint: {
		description: "The API endpoint of the service."
		required:    false
		type: string: syntax: "literal"
	}
	group_name: {
		description: """
			The [group name][group_name] of the target CloudWatch Logs stream.

			[group_name]: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/Working-with-log-groups-and-streams.html
			"""
		required: true
		type: string: syntax: "template"
	}
	region: {
		description: "The AWS region to use."
		required:    false
		type: string: syntax: "literal"
	}
	request: {
		description: "Outbound HTTP request settings."
		required:    false
		type: object: options: {
			adaptive_concurrency: {
				description: """
					Configuration of adaptive concurrency parameters.

					These parameters typically do not require changes from the default, and incorrect values can lead to meta-stable or
					unstable performance and sink behavior. Proceed with caution.
					"""
				required: false
				type: object: {
					default: {
						decrease_ratio:      0.9
						ewma_alpha:          0.4
						rtt_deviation_scale: 2.5
					}
					options: {
						decrease_ratio: {
							description: """
																The fraction of the current value to set the new concurrency limit when decreasing the limit.

																Valid values are greater than `0` and less than `1`. Smaller values cause the algorithm to scale back rapidly
																when latency increases.

																Note that the new limit is rounded down after applying this ratio.
																"""
							required: false
							type: float: default: 0.9
						}
						ewma_alpha: {
							description: """
																The weighting of new measurements compared to older measurements.

																Valid values are greater than `0` and less than `1`.

																ARC uses an exponentially weighted moving average (EWMA) of past RTT measurements as a reference to compare with
																the current RTT. Smaller values cause this reference to adjust more slowly, which may be useful if a service has
																unusually high response variability.
																"""
							required: false
							type: float: default: 0.4
						}
						rtt_deviation_scale: {
							description: """
																Scale of RTT deviations which are not considered anomalous.

																Valid values are greater than or equal to `0`, and we expect reasonable values to range from `1.0` to `3.0`.

																When calculating the past RTT average, we also compute a secondary “deviation” value that indicates how variable
																those values are. We use that deviation when comparing the past RTT average to the current measurements, so we
																can ignore increases in RTT that are within an expected range. This factor is used to scale up the deviation to
																an appropriate range.  Larger values cause the algorithm to ignore larger increases in the RTT.
																"""
							required: false
							type: float: default: 2.5
						}
					}
				}
			}
			concurrency: {
				description: "Configuration for outbound request concurrency."
				required:    false
				type: {
					string: {
						const:   "adaptive"
						default: "none"
					}
					uint: {}
				}
			}
			headers: {
				description: "Additional HTTP headers to add to every HTTP request."
				required:    false
				type: object: {
					default: {}
					options: "*": {
						description: "Additional HTTP headers to add to every HTTP request."
						required:    true
						type: string: syntax: "literal"
					}
				}
			}
			rate_limit_duration_secs: {
				description: "The time window, in seconds, used for the `rate_limit_num` option."
				required:    false
				type: uint: default: 1
			}
			rate_limit_num: {
				description: "The maximum number of requests allowed within the `rate_limit_duration_secs` time window."
				required:    false
				type: uint: default: 9223372036854775807
			}
			retry_attempts: {
				description: """
					The maximum number of retries to make for failed requests.

					The default, for all intents and purposes, represents an infinite number of retries.
					"""
				required: false
				type: uint: default: 9223372036854775807
			}
			retry_initial_backoff_secs: {
				description: """
					The amount of time to wait before attempting the first retry for a failed request.

					After the first retry has failed, the fibonacci sequence will be used to select future backoffs.
					"""
				required: false
				type: uint: default: 1
			}
			retry_max_duration_secs: {
				description: "The maximum amount of time, in seconds, to wait between retries."
				required:    false
				type: uint: default: 3600
			}
			timeout_secs: {
				description: """
					The maximum time a request can take before being aborted.

					It is highly recommended that you do not lower this value below the service’s internal timeout, as this could
					create orphaned requests, pile on retries, and result in duplicate data downstream.
					"""
				required: false
				type: uint: default: 60
			}
		}
	}
	stream_name: {
		description: """
			The [stream name][stream_name] of the target CloudWatch Logs stream.

			Note that there can only be one writer to a log stream at a time. If you have multiple
			instances of Vector writing to the same log group, you must include an identifier in the
			stream name that is guaranteed to be unique per Vector instance.

			For example, you might choose `host`.

			[stream_name]: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/Working-with-log-groups-and-streams.html
			"""
		required: true
		type: string: syntax: "template"
	}
	tls: {
		description: "TLS configuration."
		required:    false
		type: object: options: {
			alpn_protocols: {
				description: """
					Sets the list of supported ALPN protocols.

					Declare the supported ALPN protocols, which are used during negotiation with peer. Prioritized in the order
					they are defined.
					"""
				required: false
				type: array: items: type: string: syntax: "literal"
			}
			ca_file: {
				description: """
					Absolute path to an additional CA certificate file.

					The certificate must be in the DER or PEM (X.509) format. Additionally, the certificate can be provided as an inline string in PEM format.
					"""
				required: false
				type: string: syntax: "literal"
			}
			crt_file: {
				description: """
					Absolute path to a certificate file used to identify this server.

					The certificate must be in DER, PEM (X.509), or PKCS#12 format. Additionally, the certificate can be provided as
					an inline string in PEM format.

					If this is set, and is not a PKCS#12 archive, `key_file` must also be set.
					"""
				required: false
				type: string: syntax: "literal"
			}
			key_file: {
				description: """
					Absolute path to a private key file used to identify this server.

					The key must be in DER or PEM (PKCS#8) format. Additionally, the key can be provided as an inline string in PEM format.
					"""
				required: false
				type: string: syntax: "literal"
			}
			key_pass: {
				description: """
					Passphrase used to unlock the encrypted key file.

					This has no effect unless `key_file` is set.
					"""
				required: false
				type: string: syntax: "literal"
			}
			verify_certificate: {
				description: """
					Enables certificate verification.

					If enabled, certificates must be valid in terms of not being expired, as well as being issued by a trusted
					issuer. This verification operates in a hierarchical manner, checking that not only the leaf certificate (the
					certificate presented by the client/server) is valid, but also that the issuer of that certificate is valid, and
					so on until reaching a root certificate.

					Relevant for both incoming and outgoing connections.

					Do NOT set this to `false` unless you understand the risks of not verifying the validity of certificates.
					"""
				required: false
				type: bool: {}
			}
			verify_hostname: {
				description: """
					Enables hostname verification.

					If enabled, the hostname used to connect to the remote host must be present in the TLS certificate presented by
					the remote host, either as the Common Name or as an entry in the Subject Alternative Name extension.

					Only relevant for outgoing connections.

					Do NOT set this to `false` unless you understand the risks of not verifying the remote hostname.
					"""
				required: false
				type: bool: {}
			}
		}
	}
}
