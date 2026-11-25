import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import { EDGE_COLORS, LAYER_COLORS, LAYERS, NODE_RADIUS, type GraphEdge, type GraphNode, type Layer } from "./types";
import type { GraphSize } from "./graph-camera";

export type LayoutNode = GraphNode & { x?: number | undefined; y?: number | undefined };

class EdgeLines {
  readonly geometry = new THREE.BufferGeometry();
  readonly position: THREE.BufferAttribute;
  readonly color: THREE.BufferAttribute;
  readonly object: THREE.LineSegments<THREE.BufferGeometry, THREE.LineBasicMaterial>;
  private count = 0;

  constructor(capacity: number, opacity: number) {
    this.position = new THREE.BufferAttribute(new Float32Array(capacity * 6), 3).setUsage(THREE.DynamicDrawUsage);
    this.color = new THREE.BufferAttribute(new Float32Array(capacity * 6), 3).setUsage(THREE.DynamicDrawUsage);
    this.geometry.setAttribute("position", this.position);
    this.geometry.setAttribute("color", this.color);
    this.object = new THREE.LineSegments(this.geometry, new THREE.LineBasicMaterial({
      vertexColors: true, opacity, transparent: true, depthWrite: false,
    }));
    this.object.frustumCulled = false;
  }

  reset() { this.count = 0; }

  add(from: THREE.Vector3, to: THREE.Vector3, color: THREE.Color) {
    this.position.setXYZ(this.count, from.x, from.y, from.z);
    this.position.setXYZ(this.count + 1, to.x, to.y, to.z);
    this.color.setXYZ(this.count, color.r, color.g, color.b);
    this.color.setXYZ(this.count + 1, color.r, color.g, color.b);
    this.count += 2;
  }

  commit() {
    this.geometry.setDrawRange(0, this.count);
    this.position.needsUpdate = this.color.needsUpdate = true;
  }

  dispose() {
    this.object.removeFromParent();
    this.geometry.dispose();
    this.object.material.dispose();
  }
}

export class ThreeGraphScene {
  private readonly scene = new THREE.Scene();
  private readonly camera = new THREE.PerspectiveCamera(42, 1, 1, 12000);
  private readonly renderer: THREE.WebGLRenderer;
  private readonly controls: OrbitControls;
  private readonly sphere = new THREE.SphereGeometry(1, 18, 12);
  private readonly nodeMaterial = new THREE.MeshStandardMaterial({ roughness: 0.35 });
  private readonly cone = new THREE.ConeGeometry(2.7, 8, 10);
  private readonly arrowMaterial = new THREE.MeshBasicMaterial();
  private readonly plane = new THREE.PlaneGeometry(1, 1).rotateX(-Math.PI / 2);
  private readonly grid = new THREE.BufferGeometry();
  private readonly planes = LAYERS.map((layer) => {
    const surface = new THREE.Mesh(this.plane, new THREE.MeshBasicMaterial({
      color: LAYER_COLORS[layer], opacity: 0.10, transparent: true, depthWrite: false, side: THREE.DoubleSide,
    }));
    const grid = new THREE.LineSegments(this.grid, new THREE.LineBasicMaterial({
      color: LAYER_COLORS[layer], opacity: 0.38, transparent: true, depthWrite: false,
    }));
    surface.renderOrder = -2;
    grid.renderOrder = -1;
    grid.position.y = 0.15;
    const plane = new THREE.Group();
    plane.add(surface, grid);
    this.scene.add(plane);
    return plane;
  });
  private readonly selection = new THREE.Mesh(
    new THREE.RingGeometry(1.17, 1.28, 36),
    new THREE.MeshBasicMaterial({ color: 0xb1a1ff, side: THREE.DoubleSide }),
  );
  private readonly world = new Map<string, THREE.Vector3>();
  private readonly radii = new Map<string, number>();
  private readonly bounds = new THREE.Box3();
  private readonly focusBounds = new THREE.Box3();
  private readonly transform = new THREE.Object3D();
  private readonly ray = new THREE.Raycaster();
  private readonly cursor = new THREE.Vector2();
  private readonly projected = new THREE.Vector3();
  private readonly labelPoint = new THREE.Vector3();
  private readonly screen = new THREE.Vector2();
  private readonly direction = new THREE.Vector3();
  private readonly color = new THREE.Color();
  private readonly up = new THREE.Vector3(0, 1, 0);
  private readonly edgeStart = new THREE.Vector3();
  private readonly edgeEnd = new THREE.Vector3();
  private readonly darkColor = new THREE.Color(0x637083);
  private readonly defaultDirection = new THREE.Vector3(0.70, 0.66, 1).normalize();
  private nodes: LayoutNode[] = [];
  private mesh: THREE.InstancedMesh;
  private arrows: THREE.InstancedMesh;
  private normal: EdgeLines;
  private emphasis: EdgeLines;
  private edgeCapacity = 1;
  private nodeCapacity = 1;
  private frame = 0;
  private disposed = false;
  private fitted = false;
  private size = { width: 1, height: 1 };

  constructor(canvas: HTMLCanvasElement, private readonly onDraw: () => void) {
    const context = canvas.getContext("webgl2", { alpha: true, antialias: true });
    if (!context) throw new Error("WebGL 2 is unavailable");
    this.renderer = new THREE.WebGLRenderer({ canvas, context, alpha: true, antialias: true });
    this.controls = new OrbitControls(this.camera, canvas);
    this.controls.minDistance = 32;
    this.controls.maxDistance = 6000;
    this.controls.minPolarAngle = 0.08;
    this.controls.maxPolarAngle = Math.PI - 0.08;
    this.controls.listenToKeyEvents(canvas);
    this.controls.addEventListener("change", this.requestFrame);
    // The camera and layout drive updates, so an idle diagram schedules no frames.
    this.camera.position.copy(this.defaultDirection).multiplyScalar(600);
    this.controls.update();

    const key = new THREE.DirectionalLight(0xf5f7ff, 2.4);
    key.position.set(-280, 420, 340);
    this.scene.add(new THREE.HemisphereLight(0xf4f4ff, 0x717892, 1.8), key, this.selection);
    this.selection.visible = false;
    this.mesh = this.instances(this.sphere, this.nodeMaterial, this.nodeCapacity);
    this.arrows = this.instances(this.cone, this.arrowMaterial, this.edgeCapacity);
    [this.normal, this.emphasis] = this.edgeLines();

    const grid: number[] = [];
    for (let index = 0; index <= 4; index++) {
      const step = index / 4 - 0.5;
      grid.push(step, 0, -0.5, step, 0, 0.5, -0.5, 0, step, 0.5, 0, step);
    }
    this.grid.setAttribute("position", new THREE.Float32BufferAttribute(grid, 3));
  }

  private instances(geometry: THREE.BufferGeometry, material: THREE.Material, count: number) {
    const mesh = new THREE.InstancedMesh(geometry, material, count);
    mesh.count = 0;
    mesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage);
    this.scene.add(mesh);
    return mesh;
  }

  private edgeLines(): [EdgeLines, EdgeLines] {
    const normal = new EdgeLines(this.edgeCapacity * 8, 0.42);
    const emphasis = new EdgeLines(this.edgeCapacity * 8, 0.96);
    emphasis.object.renderOrder = 1;
    this.scene.add(normal.object, emphasis.object);
    return [normal, emphasis];
  }

  private grow(nodes: number, edges: number) {
    if (nodes > this.nodeCapacity) {
      this.nodeCapacity = nodes;
      this.mesh.removeFromParent();
      this.mesh.dispose();
      this.mesh = this.instances(this.sphere, this.nodeMaterial, nodes);
    }
    if (edges > this.edgeCapacity) {
      this.edgeCapacity = edges;
      this.normal.dispose();
      this.emphasis.dispose();
      this.arrows.removeFromParent();
      this.arrows.dispose();
      this.arrows = this.instances(this.cone, this.arrowMaterial, edges);
      [this.normal, this.emphasis] = this.edgeLines();
    }
  }

  updateLayout(
    nodes: LayoutNode[], edges: GraphEdge[], size: GraphSize,
    selected: string | null, highlight: string | null, neighbors: ReadonlySet<string>,
  ) {
    this.resize(size);
    this.grow(nodes.length, edges.length);
    this.nodes = nodes;
    this.bounds.makeEmpty();
    // D3's horizontal coordinates become the ground plane. Higher-level concepts sit above it.
    const gap = Math.min(128, size.height * 0.23);
    const layers = new Set<number>();
    const live = new Set<string>();
    for (const [index, node] of nodes.entries()) {
      const point = this.world.get(node.id) ?? new THREE.Vector3();
      point.set((node.x ?? size.width / 2) - size.width / 2, (1.5 - node.layer) * gap,
        (node.y ?? size.height / 2) - size.height / 2);
      this.world.set(node.id, point);
      live.add(node.id);
      layers.add(node.layer);
      this.bounds.expandByPoint(point);
      const radius = NODE_RADIUS[node.layer] * 0.54;
      this.radii.set(node.id, radius);
      this.transform.position.copy(point);
      this.transform.quaternion.identity();
      this.transform.scale.setScalar(radius);
      this.transform.updateMatrix();
      this.mesh.setMatrixAt(index, this.transform.matrix);
      this.color.set(LAYER_COLORS[node.layer]);
      if (highlight && !neighbors.has(node.id)) this.color.lerp(this.darkColor, 0.75).multiplyScalar(0.62);
      this.mesh.setColorAt(index, this.color);
    }
    for (const id of this.world.keys()) {
      if (!live.has(id)) {
        this.world.delete(id);
        this.radii.delete(id);
      }
    }
    this.mesh.count = nodes.length;
    this.mesh.instanceMatrix.needsUpdate = true;
    if (this.mesh.instanceColor) this.mesh.instanceColor.needsUpdate = true;
    this.mesh.computeBoundingSphere();
    const selectedNode = nodes.find((node) => node.id === selected);
    const selectedPoint = selectedNode && this.world.get(selectedNode.id);
    this.selection.visible = !!selectedPoint;
    if (selectedNode && selectedPoint) {
      this.selection.position.copy(selectedPoint);
      this.selection.scale.setScalar(NODE_RADIUS[selectedNode.layer] * 0.54);
    }
    this.updatePlanes(gap, layers);
    this.updateEdges(edges, highlight);
    if (!this.fitted && nodes.length) {
      this.fitted = true;
      this.fit(true);
    }
    this.requestFrame();
  }

  private resize(size: GraphSize) {
    if (size.width === this.size.width && size.height === this.size.height) return;
    this.size = size;
    this.renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
    this.renderer.setSize(size.width, size.height, false);
    this.camera.aspect = size.width / size.height;
    this.camera.updateProjectionMatrix();
  }

  private updatePlanes(gap: number, layers: Set<number>) {
    const center = this.bounds.getCenter(this.projected);
    const width = Math.max(160, this.bounds.max.x - this.bounds.min.x + 86);
    const depth = Math.max(140, this.bounds.max.z - this.bounds.min.z + 86);
    this.planes.forEach((plane, layer) => {
      plane.visible = layers.has(layer);
      if (!plane.visible) return;
      plane.position.set(center.x, (1.5 - layer) * gap - 15, center.z);
      plane.scale.set(width, 1, depth);
    });
    this.focusBounds.copy(this.bounds);
    if (layers.size) this.focusBounds.expandByVector(this.direction.set(
      (width - (this.bounds.max.x - this.bounds.min.x)) / 2, 30,
      (depth - (this.bounds.max.z - this.bounds.min.z)) / 2,
    ));
  }

  private updateEdges(edges: GraphEdge[], highlight: string | null) {
    this.normal.reset();
    this.emphasis.reset();
    let arrows = 0;
    for (const edge of edges) {
      const from = this.world.get(edge.from), to = this.world.get(edge.target);
      if (!from || !to || from.distanceToSquared(to) < 1) continue;
      const active = edge.from === highlight || edge.target === highlight;
      const batch = active ? this.emphasis : this.normal;
      this.color.set(EDGE_COLORS[edge.type]);
      if (highlight && !active) this.color.lerp(this.darkColor, 0.74).multiplyScalar(0.54);
      if (edge.type === "contradicts" || edge.type === "analogous-to") {
        const dash = edge.type === "contradicts" ? 0.67 : 0.28;
        for (let segment = 0; segment < 8; segment++) {
          batch.add(this.edgeStart.lerpVectors(from, to, segment / 8),
            this.edgeEnd.lerpVectors(from, to, (segment + dash) / 8), this.color);
        }
        continue;
      }
      batch.add(from, to, this.color);
      this.direction.subVectors(to, from).normalize();
      this.transform.position.copy(to).addScaledVector(this.direction, -19);
      this.transform.quaternion.setFromUnitVectors(this.up, this.direction);
      this.transform.scale.setScalar(active ? 1.22 : 0.92);
      this.transform.updateMatrix();
      this.arrows.setMatrixAt(arrows, this.transform.matrix);
      this.arrows.setColorAt(arrows++, this.color);
    }
    this.normal.commit();
    this.emphasis.commit();
    this.arrows.count = arrows;
    this.arrows.instanceMatrix.needsUpdate = true;
    if (this.arrows.instanceColor) this.arrows.instanceColor.needsUpdate = true;
    this.arrows.computeBoundingSphere();
  }

  pick(x: number, y: number): LayoutNode | null {
    if (!this.nodes.length) return null;
    this.camera.updateMatrixWorld();
    this.mesh.updateMatrixWorld();
    this.ray.setFromCamera(this.cursor.set(x * 2 - 1, 1 - y * 2), this.camera);
    const instance = this.ray.intersectObject(this.mesh, false)[0]?.instanceId;
    return instance === undefined ? null : this.nodes[instance] ?? null;
  }

  private project(point: THREE.Vector3) {
    this.projected.copy(point).project(this.camera);
    if (this.projected.z < -1 || this.projected.z > 1
      || Math.abs(this.projected.x) > 1 || Math.abs(this.projected.y) > 1) return null;
    return this.screen.set((this.projected.x + 1) * this.size.width / 2,
      (1 - this.projected.y) * this.size.height / 2);
  }

  projectLabel(id: string) {
    const point = this.world.get(id);
    const projected = point && this.project(point);
    if (!point || !projected) return null;
    const y = projected.y;
    this.labelPoint.set(0, -(this.radii.get(id) ?? 0), 0).applyQuaternion(this.camera.quaternion).add(point);
    const below = this.labelPoint.project(this.camera);
    return { x: projected.x, y, offset: Math.max(0, (1 - below.y) * this.size.height / 2 - y) + 4 };
  }

  projectLayer(layer: Layer) {
    const plane = this.planes[layer];
    if (!plane?.visible) return null;
    return this.project(this.labelPoint.copy(plane.position).add(
      this.direction.set(-plane.scale.x / 2, 0, plane.scale.z / 2),
    ));
  }

  setSelectionColor(cssColor: string) {
    if (cssColor) this.selection.material.color.setStyle(cssColor);
    this.requestFrame();
  }

  zoom(factor: number) {
    const offset = this.direction.subVectors(this.camera.position, this.controls.target);
    this.camera.position.copy(this.controls.target).add(offset.multiplyScalar(factor));
    this.controls.update();
    this.requestFrame();
  }

  fit(reset = false) {
    if (this.focusBounds.isEmpty()) return;
    const center = this.focusBounds.getCenter(new THREE.Vector3());
    const vertical = Math.tan(THREE.MathUtils.degToRad(this.camera.fov) / 2);
    const horizontal = vertical * this.camera.aspect;
    const orientation = reset ? this.defaultDirection
      : this.direction.subVectors(this.camera.position, this.controls.target).normalize();
    const right = new THREE.Vector3().crossVectors(this.up, orientation).normalize();
    const up = new THREE.Vector3().crossVectors(orientation, right);
    const corner = new THREE.Vector3();
    const { min, max } = this.focusBounds;
    let distance = 0;
    // Fit each corner against the camera's horizontal and vertical frustum.
    for (const x of [min.x, max.x]) {
      for (const y of [min.y, max.y]) {
        for (const z of [min.z, max.z]) {
          corner.set(x, y, z).sub(center);
          const margin = Math.max(Math.abs(corner.dot(right)) / horizontal, Math.abs(corner.dot(up)) / vertical);
          distance = Math.max(distance, corner.dot(orientation) + margin * 1.17);
        }
      }
    }
    this.controls.target.copy(center);
    this.camera.position.copy(center).addScaledVector(orientation, distance);
    this.controls.update();
    this.requestFrame();
  }

  focus(id: string, zoom = true) {
    const point = this.world.get(id);
    if (!point) return;
    const offset = this.direction.subVectors(this.camera.position, this.controls.target);
    if (zoom) offset.setLength(Math.max(80, offset.length() * 0.64));
    this.controls.target.copy(point);
    this.camera.position.copy(point).add(offset);
    this.controls.update();
    this.requestFrame();
  }

  requestFrame = () => {
    if (this.disposed || this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.selection.quaternion.copy(this.camera.quaternion);
      this.renderer.render(this.scene, this.camera);
      this.onDraw();
    });
  };

  dispose() {
    this.disposed = true;
    cancelAnimationFrame(this.frame);
    this.controls.dispose();
    this.normal.dispose();
    this.emphasis.dispose();
    this.mesh.dispose();
    this.arrows.dispose();
    this.sphere.dispose();
    this.nodeMaterial.dispose();
    this.arrowMaterial.dispose();
    this.cone.dispose();
    this.grid.dispose();
    this.plane.dispose();
    this.selection.geometry.dispose();
    this.selection.material.dispose();
    for (const plane of this.planes) {
      for (const surface of plane.children) {
        if (surface instanceof THREE.Mesh || surface instanceof THREE.LineSegments) surface.material.dispose();
      }
    }
    this.renderer.dispose();
  }
}
