// The transparent tie check (tools/checks/transparent-ties.sh): two pairs of half-clear quads, red and blue, under a
// camera that swings ±35°. Pair one is two coincident quads (an exact tie: only the draw order decides which is on top),
// pair two crossed quads 0.3 mm apart (a near tie, as layered lace rings or letters over a plate are). The tie's top
// colour must never change; the near pair's may change once, where the camera passes the point their distances cross.
import React from "react";
import { AbsoluteFill, Composition, registerRoot, useCurrentFrame } from "raea";
import { Canvas3D } from "raea/3d";
import { Camera3d } from "raea/camera";
import { Mesh3d, Plane3d } from "raea/mesh";
import { StandardMaterial } from "raea/pbr";
import { Srgba } from "raea/color";
import { Quat, Transform } from "raea/transform";

const N = 60;
function Quad({ name, at, color, turn }: { name: string; at: [number, number, number]; color: string; turn: number }) {
  return (
    <Mesh3d name={name} transform={Transform.fromXyz(...at).withRotation(Quat.fromEuler("XYZ", Math.PI / 2, 0, turn))}>
      <Plane3d half_size={[1.6, 1.6]} />
      <StandardMaterial base_color={Srgba.hex(color).with_alpha(0.5)} alpha_mode="Blend" unlit double_sided cull_mode={null} />
    </Mesh3d>
  );
}
function Ties() {
  const f = useCurrentFrame();
  const a = (f / (N - 1) - 0.5) * 2 * (35 * Math.PI) / 180;
  return (
    <AbsoluteFill style={{ background: "#000" }}>
      <Canvas3D>
        <Camera3d transform={Transform.fromXyz(6 * Math.sin(a), 0, 6 * Math.cos(a)).lookingAt([0, 0, 0])} projection={{ Perspective: { fov: 0.6 } }} />
        {/* gone for frames 20-25 and back: Bevy puts a returning part at the end of the phase, so a tie's order changed */}
        {(f < 20 || f >= 26) && <Quad name="tie-red" at={[-1, 0, 0]} color="#ff0000" turn={0.3} />}
        <Quad name="tie-blue" at={[-1, 0, 0]} color="#0000ff" turn={0.3} />
        <Quad name="near-red" at={[1, 0, 0.0003]} color="#ff0000" turn={0.3} />
        <Quad name="near-blue" at={[1, 0, 0]} color="#0000ff" turn={-0.3} />
      </Canvas3D>
    </AbsoluteFill>
  );
}
registerRoot(() => <Composition id="transparent-ties" component={Ties} width={640} height={360} fps={30} durationInFrames={N} />);
