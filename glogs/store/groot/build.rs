extern crate protoc_grpcio;

fn main() {
    let proto_root = "../../proto";
    protoc_grpcio::compile_grpc_protos(
        &[
            proto_root.to_owned() + "/sdk/model.proto",
            proto_root.to_owned() + "/sdk/common.proto",
            proto_root.to_owned() + "/sdk/schema.proto",
        ],
        &[proto_root],
        "./src/db/proto",
        None,
    )
    .expect("Failed to compile gRPC definitions!");
}
