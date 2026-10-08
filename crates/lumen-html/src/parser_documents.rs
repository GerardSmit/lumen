use alloc::{boxed::Box,collections::BTreeMap,string::String,vec::Vec};
use crate::{Document,DocumentMode,Error,Name,Namespace,NodeId,NodeKind};
use crate::shadow::ShadowOptions;

/// Borrowed tree-builder access. Ordinary document parsers are monomorphized
/// directly against Document; rare adopted open elements use ParserDocuments.
pub trait ParserDocument {
    fn current_node(&self,node:NodeId)->NodeId { node }
    fn identity_revision(&self)->usize {0}
    fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), Error>;
    fn append_data(&mut self, id: NodeId, data: &str) -> Result<(), Error>;
    fn associate_parser_form(&self, control: NodeId, form: NodeId) -> Result<(), Error>;
    fn attach_shadow_with_options( &mut self, host: NodeId, options: ShadowOptions) -> Result<NodeId, Error>;
    fn clone_shallow_at(&mut self,parent:NodeId,source:NodeId)->Result<NodeId,Error>;
    fn clone_shallow(&mut self, source: NodeId) -> Result<NodeId, Error>;
    fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error>;
    fn create(&mut self, kind: NodeKind) -> Result<NodeId, Error>;
    fn create_unprefixed_element(&mut self, namespace: Namespace, name: Name, attributes: Vec<(Name, String)>) -> Result<NodeId, Error>;
    fn create_with_is_value(&mut self, kind: NodeKind, is_value: Option<&str>) -> Result<NodeId, Error>;
    fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error>;
    fn detach(&mut self, child: NodeId) -> Result<Option<NodeId>, Error>;
    fn document_mode(&self) -> DocumentMode;
    fn first_child(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn get_attribute_ns_ref( &self, id: NodeId, namespace_uri: Option<&str>, local_name: &str) -> Result<Option<&str>, Error>;
    fn initialize_parser_is_value(&mut self, node: NodeId, is_value: Option<&str>) -> Result<(), Error>;
    fn insert_before( &mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<(), Error>;
    fn kind(&self, id: NodeId) -> Result<&NodeKind, Error>;
    fn last_child(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn next_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn node_document(&self, node: NodeId) -> Result<NodeId, Error>;
    fn parent(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn parser_custom_element_defined(&self, context: NodeId, local_name: &str, is_value: Option<&str>) -> bool;
    fn previous_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn record_parser_element_birth(&self, element: NodeId, intended_parent: NodeId, document_parser: bool) -> Result<(), Error>;
    fn record_parser_element_completion(&self, node: NodeId);
    fn record_parser_style_block_update(&self,node:NodeId)->Result<(),Error>;
    fn root(&self) -> NodeId;
    fn root_node(&self, node: NodeId, composed: bool) -> Result<NodeId, Error>;
    fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> Result<(), Error>;
    fn set_attribute_ns(&mut self,id:NodeId,namespace_uri:Option<&str>,qualified_name:&str,value:&str)->Result<(),Error>;
    fn set_attribute_namespace_metadata( &mut self, id: NodeId, qualified_name: &str, namespace_uri: Option<&str>) -> Result<(), Error>;
    fn set_doctype_identifiers( &mut self, id: NodeId, public_id: &str, system_id: &str) -> Result<(), Error>;
    fn set_document_mode(&mut self, mode: DocumentMode);
    fn set_node_document(&mut self, node: NodeId, owner: NodeId) -> Result<(), Error>;
    fn shadow_root(&self, host: NodeId) -> Result<Option<NodeId>, Error>;
    fn template_content(&self, id: NodeId) -> Result<Option<NodeId>, Error>;
    fn is_host_including_inclusive_ancestor(&self, ancestor: NodeId, node: NodeId) -> Result<bool, Error>;
    fn create_at(&mut self,parent:NodeId,kind:NodeKind,is_value:Option<&str>)->Result<NodeId,Error>;
    fn create_unprefixed_at(&mut self,parent:NodeId,namespace:Namespace,name:Name,attributes:Vec<(Name,String)>)->Result<NodeId,Error>;
}
impl ParserDocument for Document {
    fn clone_shallow_at(&mut self,parent:NodeId,source:NodeId)->Result<NodeId,Error> {
        let owner=self.node_document(parent)?;let copy=self.clone_shallow(source)?;self.set_node_document(copy,owner)?;Ok(copy)
    }

    fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), Error> { Document::append(self ,parent, child) }
    fn append_data(&mut self, id: NodeId, data: &str) -> Result<(), Error> { Document::append_data(self ,id, data) }
    fn associate_parser_form(&self, control: NodeId, form: NodeId) -> Result<(), Error> { Document::associate_parser_form(self ,control, form) }
    fn attach_shadow_with_options( &mut self, host: NodeId, options: ShadowOptions) -> Result<NodeId, Error> { Document::attach_shadow_with_options(self ,host, options) }
    fn clone_shallow(&mut self, source: NodeId) -> Result<NodeId, Error> { Document::clone_shallow(self ,source) }
    fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error> { Document::clone_subtree(self ,source) }
    fn create(&mut self, kind: NodeKind) -> Result<NodeId, Error> { Document::create(self ,kind) }
    fn create_unprefixed_element(&mut self, namespace: Namespace, name: Name, attributes: Vec<(Name, String)>) -> Result<NodeId, Error> { Document::create_unprefixed_element(self ,namespace, name, attributes) }
    fn create_with_is_value(&mut self, kind: NodeKind, is_value: Option<&str>) -> Result<NodeId, Error> { Document::create_with_is_value(self ,kind, is_value) }
    fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error> { Document::destroy_subtree(self ,root) }
    fn detach(&mut self, child: NodeId) -> Result<Option<NodeId>, Error> { Document::detach(self ,child) }
    fn document_mode(&self) -> DocumentMode { Document::document_mode(self) }
    fn first_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::first_child(self ,id) }
    fn get_attribute_ns_ref( &self, id: NodeId, namespace_uri: Option<&str>, local_name: &str) -> Result<Option<&str>, Error> { Document::get_attribute_ns_ref(self ,id, namespace_uri, local_name) }
    fn initialize_parser_is_value(&mut self, node: NodeId, is_value: Option<&str>) -> Result<(), Error> { Document::initialize_parser_is_value(self ,node, is_value) }
    fn insert_before( &mut self, parent: NodeId, child: NodeId, before: Option<NodeId>) -> Result<(), Error> { Document::insert_before(self ,parent, child, before) }
    fn kind(&self, id: NodeId) -> Result<&NodeKind, Error> { Document::kind(self ,id) }
    fn last_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::last_child(self ,id) }
    fn next_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::next_sibling(self ,id) }
    fn node_document(&self, node: NodeId) -> Result<NodeId, Error> { Document::node_document(self ,node) }
    fn parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::parent(self ,id) }
    fn parser_custom_element_defined(&self, context: NodeId, local_name: &str, is_value: Option<&str>) -> bool { Document::parser_custom_element_defined(self ,context, local_name, is_value) }
    fn previous_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::previous_sibling(self ,id) }
    fn record_parser_element_birth(&self, element: NodeId, intended_parent: NodeId, document_parser: bool) -> Result<(), Error> { Document::record_parser_element_birth(self ,element, intended_parent, document_parser) }
    fn record_parser_element_completion(&self, node: NodeId) { Document::record_parser_element_completion(self, node); }
    fn record_parser_style_block_update(&self,node:NodeId)->Result<(),Error> {Document::record_parser_style_block_update(self,node)}
    fn root(&self) -> NodeId { Document::root(self) }
    fn root_node(&self, node: NodeId, composed: bool) -> Result<NodeId, Error> { Document::root_node(self ,node, composed) }
    fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> Result<(), Error> { Document::set_attribute(self ,id, name, value) }
    fn set_attribute_ns(&mut self,id:NodeId,namespace_uri:Option<&str>,qualified_name:&str,value:&str)->Result<(),Error> {Document::set_attribute_ns(self,id,namespace_uri,qualified_name,value)}
    fn set_attribute_namespace_metadata( &mut self, id: NodeId, qualified_name: &str, namespace_uri: Option<&str>) -> Result<(), Error> { Document::set_attribute_namespace_metadata(self ,id, qualified_name, namespace_uri) }
    fn set_doctype_identifiers( &mut self, id: NodeId, public_id: &str, system_id: &str) -> Result<(), Error> { Document::set_doctype_identifiers(self ,id, public_id, system_id) }
    fn set_document_mode(&mut self, mode: DocumentMode) { Document::set_document_mode(self ,mode) }
    fn set_node_document(&mut self, node: NodeId, owner: NodeId) -> Result<(), Error> { Document::set_node_document(self ,node, owner) }
    fn shadow_root(&self, host: NodeId) -> Result<Option<NodeId>, Error> { Document::shadow_root(self ,host) }
    fn template_content(&self, id: NodeId) -> Result<Option<NodeId>, Error> { Document::template_content(self ,id) }
    fn is_host_including_inclusive_ancestor(&self, ancestor: NodeId, node: NodeId) -> Result<bool, Error> { Document::is_host_including_inclusive_ancestor(self ,ancestor, node) }
    fn create_at(&mut self,parent:NodeId,kind:NodeKind,is_value:Option<&str>)->Result<NodeId,Error> {
        let owner=self.node_document(parent)?;
        let node=self.create_with_is_value(kind,is_value)?;
        self.set_node_document(node,owner)?;
        Ok(node)
    }
    fn create_unprefixed_at(&mut self,parent:NodeId,namespace:Namespace,name:Name,attributes:Vec<(Name,String)>)->Result<NodeId,Error> {
        let owner=self.node_document(parent)?;
        let node=self.create_unprefixed_element(namespace,name,attributes)?;
        self.set_node_document(node,owner)?;
        Ok(node)
    }
}

/// One actual adoption committed by tree construction. Native owners consume
/// these after the borrowed feed and before reactions or author callbacks.
pub struct ParserAdoption {
    pub source_document:NodeId,
    pub target_document:NodeId,
    pub mapping:Vec<(NodeId,NodeId)>,
    pub documents:Vec<(NodeId,NodeId,NodeId)>,
    pub publication_complete:bool,
}

/// Operation-local arena borrows. The source document remains the parser's
/// document; open elements and insertion locations may belong to other arenas.
pub struct ParserDocuments<'a> {
    documents:Vec<&'a mut Document>,
    owners:BTreeMap<u64,usize>,
    aliases:BTreeMap<u128,NodeId>,
    adoptions:Vec<ParserAdoption>,
    node_limit:usize,
    metadata_bytes:usize,
    adoption_publication:Option<Box<dyn FnMut(&Document,&Document,&ParserAdoption)->Result<(),Error>+'a>>,
}
impl<'a> ParserDocuments<'a> {
    pub fn new(documents:Vec<&'a mut Document>)->Result<Self,Error> {
        if documents.is_empty() {return Err(Error::InvalidNode);}
        let metadata_bytes=documents.capacity().checked_mul(core::mem::size_of::<&mut Document>()).and_then(|bytes|bytes.checked_add(documents.len().checked_mul(core::mem::size_of::<(u64,usize)>()+4*core::mem::size_of::<usize>())?)).ok_or(Error::LimitExceeded)?;
        if metadata_bytes>crate::html::MAX_HTML_BYTES {return Err(Error::LimitExceeded);}
        let mut owners=BTreeMap::new();
        for (index,document) in documents.iter().enumerate() {
            if owners.insert(document.root().document_id(),index).is_some() {return Err(Error::InvalidNode);}
        }
        let node_limit=documents.iter().try_fold(0usize,|total,document|total.checked_add(document.max_nodes)).ok_or(Error::LimitExceeded)?;
        Ok(Self {documents,owners,aliases:BTreeMap::new(),adoptions:Vec::new(),node_limit,metadata_bytes,adoption_publication:None})
    }
    /// Publish native plain state before target insertion observers see the new
    /// identity. This operation-local hook never invokes authored callbacks.
    pub fn set_adoption_publication(&mut self,publication:Box<dyn FnMut(&Document,&Document,&ParserAdoption)->Result<(),Error>+'a>) {
        self.adoption_publication=Some(publication);
    }
    pub fn take_adoptions(&mut self)->Vec<ParserAdoption> {core::mem::take(&mut self.adoptions)}
    fn resolve(&self,mut node:NodeId)->NodeId {
        for _ in 0..self.aliases.len() {
            let Some(&next)=self.aliases.get(&node.key()) else {break;};
            if next==node {break;}
            node=next;
        }
        node
    }
    fn index(&self,node:NodeId)->Result<usize,Error> {
        let node=self.resolve(node);
        if node.document_id()==self.documents[0].root().document_id() {return Ok(0);}
        self.owners.get(&node.document_id()).copied().ok_or(Error::InvalidNode)
    }
    fn read(&self,node:NodeId)->Result<&Document,Error> {Ok(&*self.documents[self.index(node)?])}
    fn write(&mut self,node:NodeId)->Result<&mut Document,Error> {
        let index=self.index(node)?;Ok(&mut *self.documents[index])
    }
    fn pair(&mut self,source:usize,target:usize)->(&mut Document,&mut Document) {
        if source<target {let (left,right)=self.documents.split_at_mut(target);(&mut *left[source],&mut *right[0])}
        else {let (left,right)=self.documents.split_at_mut(source);(&mut *right[0],&mut *left[target])}
    }
}
impl crate::graph::DocumentGraph for ParserDocuments<'_> {
    type Read<'a>=&'a Document where Self:'a;
    fn read(&self,node:NodeId)->Result<Self::Read<'_>,Error> {self.read(node)}
    fn node_limit(&self)->usize {self.node_limit}
}
impl ParserDocument for ParserDocuments<'_> {
    fn clone_shallow_at(&mut self,parent:NodeId,source:NodeId)->Result<NodeId,Error> {
        let parent=self.resolve(parent);let source=self.resolve(source);
        let target_index=self.index(parent)?;let source_index=self.index(source)?;
        let owner=self.documents[target_index].node_document(parent)?;
        let copy=if target_index==source_index {self.documents[target_index].clone_shallow(source)?}
            else {let (source_document,target_document)=self.pair(source_index,target_index);target_document.clone_shallow_from(source_document,source)?};
        self.documents[target_index].set_node_document(copy,owner)?;Ok(copy)
    }

    fn current_node(&self,node:NodeId)->NodeId {self.resolve(node)}
    fn identity_revision(&self)->usize {self.adoptions.len()}
    fn append_data(&mut self, id: NodeId, data: &str) -> Result<(), Error> {let id=self.resolve(id);self.write(id)?.append_data(id, data)}
    fn attach_shadow_with_options( &mut self, host: NodeId, options: ShadowOptions) -> Result<NodeId, Error> {let host=self.resolve(host);self.write(host)?.attach_shadow_with_options(host, options)}
    fn clone_shallow(&mut self, source: NodeId) -> Result<NodeId, Error> {let source=self.resolve(source);self.write(source)?.clone_shallow(source)}
    fn clone_subtree(&mut self, source: NodeId) -> Result<NodeId, Error> {let source=self.resolve(source);self.write(source)?.clone_subtree(source)}
    fn destroy_subtree(&mut self, root: NodeId) -> Result<(), Error> {let root=self.resolve(root);self.write(root)?.destroy_subtree(root)}
    fn detach(&mut self, child: NodeId) -> Result<Option<NodeId>, Error> {let child=self.resolve(child);self.write(child)?.detach(child)}
    fn document_mode(&self) -> DocumentMode {self.documents[0].document_mode()}
    fn first_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.first_child(id)}
    fn get_attribute_ns_ref( &self, id: NodeId, namespace_uri: Option<&str>, local_name: &str) -> Result<Option<&str>, Error> {let id=self.resolve(id);self.read(id)?.get_attribute_ns_ref(id, namespace_uri, local_name)}
    fn initialize_parser_is_value(&mut self, node: NodeId, is_value: Option<&str>) -> Result<(), Error> {let node=self.resolve(node);self.write(node)?.initialize_parser_is_value(node, is_value)}
    fn kind(&self, id: NodeId) -> Result<&NodeKind, Error> {let id=self.resolve(id);self.read(id)?.kind(id)}
    fn last_child(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.last_child(id)}
    fn next_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.next_sibling(id)}
    fn node_document(&self, node: NodeId) -> Result<NodeId, Error> {let node=self.resolve(node);self.read(node)?.node_document(node)}
    fn parent(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.parent(id)}
    fn previous_sibling(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.previous_sibling(id)}
    fn root(&self) -> NodeId {self.documents[0].root()}
    fn root_node(&self, node: NodeId, composed: bool) -> Result<NodeId, Error> {let node=self.resolve(node);self.read(node)?.root_node(node, composed)}
    fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> Result<(), Error> {let id=self.resolve(id);self.write(id)?.set_attribute(id, name, value)}
    fn set_attribute_ns(&mut self,id:NodeId,namespace_uri:Option<&str>,qualified_name:&str,value:&str)->Result<(),Error> {let id=self.resolve(id);self.write(id)?.set_attribute_ns(id,namespace_uri,qualified_name,value)}
    fn set_attribute_namespace_metadata( &mut self, id: NodeId, qualified_name: &str, namespace_uri: Option<&str>) -> Result<(), Error> {let id=self.resolve(id);self.write(id)?.set_attribute_namespace_metadata(id, qualified_name, namespace_uri)}
    fn set_doctype_identifiers( &mut self, id: NodeId, public_id: &str, system_id: &str) -> Result<(), Error> {let id=self.resolve(id);self.write(id)?.set_doctype_identifiers(id, public_id, system_id)}
    fn set_document_mode(&mut self, mode: DocumentMode) {self.documents[0].set_document_mode(mode)}
    fn shadow_root(&self, host: NodeId) -> Result<Option<NodeId>, Error> {let host=self.resolve(host);self.read(host)?.shadow_root(host)}
    fn template_content(&self, id: NodeId) -> Result<Option<NodeId>, Error> {let id=self.resolve(id);self.read(id)?.template_content(id)}
    fn create(&mut self,kind:NodeKind)->Result<NodeId,Error> {self.documents[0].create(kind)}
    fn create_with_is_value(&mut self,kind:NodeKind,is_value:Option<&str>)->Result<NodeId,Error> {self.documents[0].create_with_is_value(kind,is_value)}
    fn create_unprefixed_element(&mut self,namespace:Namespace,name:Name,attributes:Vec<(Name,String)>)->Result<NodeId,Error> {self.documents[0].create_unprefixed_element(namespace,name,attributes)}
    fn create_at(&mut self,parent:NodeId,kind:NodeKind,is_value:Option<&str>)->Result<NodeId,Error> {
        let parent=self.resolve(parent);ParserDocument::create_at(self.write(parent)?,parent,kind,is_value)
    }
    fn create_unprefixed_at(&mut self,parent:NodeId,namespace:Namespace,name:Name,attributes:Vec<(Name,String)>)->Result<NodeId,Error> {
        let parent=self.resolve(parent);ParserDocument::create_unprefixed_at(self.write(parent)?,parent,namespace,name,attributes)
    }
    fn set_node_document(&mut self,node:NodeId,owner:NodeId)->Result<(),Error> {
        let node=self.resolve(node);let owner=self.resolve(owner);self.write(node)?.set_node_document(node,owner)
    }
    fn associate_parser_form(&self,control:NodeId,form:NodeId)->Result<(),Error> {
        let control=self.resolve(control);let form=self.resolve(form);
        self.read(form)?.kind(form)?;
        self.read(control)?.associate_parser_form_validated(control,form)
    }
    fn record_parser_element_birth(&self,element:NodeId,intended_parent:NodeId,document_parser:bool)->Result<(),Error> {
        let element=self.resolve(element);let intended_parent=self.resolve(intended_parent);
        self.read(element)?.record_parser_element_birth(element,intended_parent,document_parser)
    }
    fn record_parser_style_block_update(&self,node:NodeId)->Result<(),Error> {
        let node=self.resolve(node);self.read(node)?.record_parser_style_block_update(node)
    }
    fn record_parser_element_completion(&self, node: NodeId) {
        let node = self.resolve(node);
        if let Ok(document) = self.read(node) {
            document.record_parser_element_completion(node);
        }
    }
    fn parser_custom_element_defined(&self,context:NodeId,local_name:&str,is_value:Option<&str>)->bool {
        let context=self.resolve(context);
        self.read(context).is_ok_and(|document|document.parser_custom_element_defined(context,local_name,is_value))
    }
    fn is_host_including_inclusive_ancestor(&self,ancestor:NodeId,node:NodeId)->Result<bool,Error> {
        crate::graph::host_including_ancestor(self,self.resolve(ancestor),self.resolve(node))
    }
    fn append(&mut self,parent:NodeId,child:NodeId)->Result<(),Error> {self.insert_before(parent,child,None)}
    fn insert_before(&mut self,parent:NodeId,child:NodeId,before:Option<NodeId>)->Result<(),Error> {
        let parent=self.resolve(parent);let child=self.resolve(child);let before=before.map(|node|self.resolve(node));
        let source=self.index(child)?;let target=self.index(parent)?;
        self.documents[target].validate_parser_insert_from(self.documents[source],parent,child,before,
            |ancestor,node|crate::graph::host_including_ancestor(self,ancestor,node))?;
        let before=if before==Some(child) {self.documents[source].next_sibling(child)?}else {before};
        let child=if source==target {child}else {
            let source_document=self.documents[source].root();let target_document=self.documents[target].root();
            let remaining=crate::html::MAX_HTML_BYTES.checked_sub(self.metadata_bytes).ok_or(Error::LimitExceeded)?;
            let nodes=crate::graph::identity_nodes_bounded(self,child,remaining)?;
            let count=nodes.len();
            // Account the mapping, logical-owner snapshots, aliases and temporary
            // sorted lookup before either arena is mutated. B-tree node overhead
            // and the Vec minimum allocation are conservatively charged.
            let entry_bytes=core::mem::size_of::<NodeId>()*2+core::mem::size_of::<(NodeId,NodeId)>()*2
                +core::mem::size_of::<(NodeId,NodeId,NodeId)>()*2
                +core::mem::size_of::<(NodeId,NodeId)>()+4*core::mem::size_of::<usize>();
            let bytes=count.checked_mul(entry_bytes)
                .and_then(|bytes|bytes.checked_add(core::mem::size_of::<ParserAdoption>()*4))
                .and_then(|bytes|bytes.checked_add(self.metadata_bytes)).ok_or(Error::LimitExceeded)?;
            if bytes>crate::html::MAX_HTML_BYTES {return Err(Error::LimitExceeded);}
            self.adoptions.try_reserve(1).map_err(|_|Error::LimitExceeded)?;
            let mut old_documents=Vec::new();old_documents.try_reserve_exact(count).map_err(|_|Error::LimitExceeded)?;
            for node in nodes {
                if node.document_id()==source_document.document_id() {old_documents.push((node,self.documents[source].node_document(node)?));}
            }
            old_documents.sort_unstable_by_key(|(node,_)|node.key());
            let mut documents=Vec::new();documents.try_reserve_exact(count).map_err(|_|Error::LimitExceeded)?;
            let (adopted,mapping)={
                let (source_document_mut,target_document_mut)=self.pair(source,target);
                target_document_mut.adopt_subtree_from(source_document_mut,child)?
            };
            // Once adoption mutates either arena, retain its mapping even if a
            // later owner-link allocation or insertion fails. Native identity
            // migration must complete for the committed adoption on error paths.
            self.adoptions.push(ParserAdoption {source_document,target_document,mapping,documents,publication_complete:false});
            self.metadata_bytes=bytes;
            let adoption=self.adoptions.last_mut().ok_or(Error::InvalidNode)?;
            for &(old,new) in &adoption.mapping {self.aliases.insert(old.key(),new);}
            // Preserve mapping order for native completion's O(1) owner tuple
            // lookup. The source graph's traversal order need not equal the
            // physical arena migration order, so resolve old owners by a sorted
            // temporary index instead of leaving an O(n²) fallback scan.
            for &(old,new) in &adoption.mapping {
                let index=old_documents.binary_search_by_key(&old.key(),|(candidate,_)|candidate.key()).map_err(|_|Error::InvalidNode)?;
                adoption.documents.push((old,old_documents[index].1,self.documents[target].node_document(new)?));
            }
            for document in &mut self.documents {document.remap_foreign_template_links(&adoption.mapping)?;}
            if let Some(publication)=self.adoption_publication.as_mut() {
                publication(self.documents[source],self.documents[target],adoption)?;
                adoption.publication_complete=true;
            }
            adopted
        };
        self.documents[target].insert_parser_validated(parent,child,before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::Cell;

    #[test]
    fn specification_parser_owner_view_identity_traversal_admits_capacity_before_growth() {
        let mut document=Document::new(128);
        let fragment=document.create(NodeKind::DocumentFragment).unwrap();
        for _ in 0..32 {
            let text=document.create(NodeKind::Text(String::from("source"))).unwrap();
            document.append(fragment,text).unwrap();
        }
        let version=document.version();let nodes=document.node_count();
        let minimum_capacity=8*core::mem::size_of::<NodeId>();
        assert_eq!(crate::graph::identity_nodes_bounded(&document,fragment,minimum_capacity),Err(Error::LimitExceeded));
        assert_eq!(document.version(),version);assert_eq!(document.node_count(),nodes);
        assert_eq!(crate::graph::identity_nodes_bounded(&document,fragment,4096).unwrap().len(),33);
    }

    #[test]
    fn specification_parser_owner_view_adoption_publication_and_error_identity() {
        let mut source=Document::new(64);let mut target=Document::new(64);
        let child=source.create_unprefixed_element(Namespace::Html,Name::new("section"),Vec::new()).unwrap();
        source.append(source.root(),child).unwrap();
        let target_root=target.root();
        let published=Rc::new(Cell::new(false));let phase=published.clone();
        let adoption={
            let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
            view.set_adoption_publication(Box::new(move|source,target,adoption| {
                let new=adoption.mapping.iter().find(|(old,_)|*old==child).unwrap().1;
                assert!(source.kind(child).is_err());assert!(target.kind(new).is_ok());
                assert_eq!(target.parent(new).unwrap(),None,"publication precedes insertion");
                phase.set(true);Ok(())
            }));
            view.append(target_root,child).unwrap();
            assert!(published.get());
            let current=view.current_node(child);
            assert_eq!(view.parent(current).unwrap(),Some(target_root));
            let mut adoptions=view.take_adoptions();assert_eq!(adoptions.len(),1);
            adoptions.remove(0)
        };
        assert!(adoption.publication_complete);
        assert_eq!(adoption.documents[0].1,source.root());
        assert_eq!(adoption.documents[0].2,target.root());

        // A failed publication cannot undo committed DOM adoption. Completion
        // still receives the exact mapping needed to rebind native identities.
        let child=source.create_unprefixed_element(Namespace::Html,Name::new("aside"),Vec::new()).unwrap();
        let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
        view.set_adoption_publication(Box::new(|_,_,_|Err(Error::LimitExceeded)));
        assert_eq!(view.append(target_root,child),Err(Error::Hierarchy));
        assert_eq!(view.current_node(child),child);assert!(view.take_adoptions().is_empty());
        assert_eq!(view.parent(child).unwrap(),None);
        let existing=view.first_child(target_root).unwrap().unwrap();
        assert_eq!(view.append(existing,child),Err(Error::LimitExceeded));
        let current=view.current_node(child);
        assert_ne!(current,child);assert_eq!(view.parent(current).unwrap(),None);
        let adoptions=view.take_adoptions();assert_eq!(adoptions.len(),1);
        assert!(!adoptions[0].publication_complete);
    }

    #[test]
    fn specification_parser_owner_view_form_pointer_survives_detached_control_adoption() {
        let mut source=Document::new(32);let mut target=Document::new(32);
        let control=source.create_unprefixed_element(Namespace::Html,Name::new("input"),Vec::new()).unwrap();
        let form=target.create_unprefixed_element(Namespace::Html,Name::new("form"),Vec::new()).unwrap();
        target.append(target.root(),form).unwrap();
        let current={
            let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
            view.associate_parser_form(control,form).unwrap();
            view.append(form,control).unwrap();view.current_node(control)
        };
        assert_eq!(crate::forms::form_owner(&target,current),Some(form));
        target.remove(current).unwrap();
        assert_eq!(target.parser_form_owner(current),None,"ordinary removal resets parser association");
    }

    #[test]
    fn specification_parser_owner_view_continues_foreign_open_elements_through_eof() {
        use crate::html::{HtmlDocumentParser,ParseOptions};
        let mut source=Document::new(128);let mut target=Document::new(128);
        let (mut parser,_)=HtmlDocumentParser::open(&mut source,ParseOptions {allow_declarative_shadow_roots:false,scripting_enabled:true}).unwrap();
        let script=parser.write_final_until_script(&mut source,"<!doctype html><section id=moved><b><script></script>foreign</b><i>tail</i></section><p id=source>source</p>").unwrap();
        assert!(script.is_some());
        let source_root=source.root();
        let section=crate::selector::query_selector(&source,source_root,"#moved").unwrap().unwrap();
        let (moved,mapping)=target.adopt_subtree_from(&mut source,section).unwrap();
        target.append(target.root(),moved).unwrap();
        parser.project_retained_nodes(|node|mapping.iter().find_map(|(old,new)|(*old==node).then_some(*new)).unwrap_or(node));
        let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
        assert!(parser.resume_until_script(&mut view).unwrap().is_none());
        drop(view);
        let bold=crate::selector::query_selector(&target,target.root(),"b").unwrap().unwrap();
        let text=target.last_child(bold).unwrap().unwrap();
        assert!(matches!(target.kind(text),Ok(NodeKind::Text(value)) if value=="foreign"));
        assert!(crate::selector::query_selector(&target,target.root(),"i").unwrap().is_some());
        assert!(crate::selector::query_selector(&source,source_root,"#source").unwrap().is_some());
        assert!(crate::selector::query_selector(&source,source_root,"#moved").unwrap().is_none());
    }
    #[test]
    fn specification_parser_owner_view_pending_insertion_reprojects_committed_adoption() {
        use crate::html::{HtmlDocumentParser,ParseOptions};
        use alloc::rc::Rc;
        let mut source=Document::new(128);let mut target=Document::new(128);
        source.set_parser_custom_element_predicate(Some(Rc::new(|_,_,local,_|local=="x-pending")));
        let (mut parser,_)=HtmlDocumentParser::open(&mut source,ParseOptions {allow_declarative_shadow_roots:false,scripting_enabled:true}).unwrap();
        assert!(parser.write_final_until_script(&mut source,"<x-pending id=moved>text</x-pending><p id=tail>source</p>").unwrap().is_none());
        let old=parser.pending_element().expect("defined custom creation is suspended");
        parser.append_pending_element_attributes(&mut source).unwrap();
        let (foreign,mapping)=target.adopt_subtree_from(&mut source,old).unwrap();
        parser.project_retained_nodes(|node|mapping.iter().find_map(|(old,new)|(*old==node).then_some(*new)).unwrap_or(node));
        assert_eq!(parser.pending_element(),Some(foreign));
        let current={let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
            parser.insert_pending_element(&mut view).unwrap();let current=view.current_node(foreign);
            assert_ne!(current,foreign);assert_eq!(view.take_adoptions().len(),1);current};
        parser.visit_retained_nodes(|node|assert!(source.kind(node).is_ok(),"retained parser identity must use the committed source arena"));
        assert!(parser.resume_after_pending_element(&mut source).unwrap().is_none());
        assert_eq!(crate::selector::query_selector(&source,source.root(),"#moved").unwrap(),Some(current));
        assert!(crate::selector::query_selector(&source,source.root(),"#tail").unwrap().is_some());
    }

    #[test]
    fn specification_parser_owner_view_xml_namespaces_character_data_and_eof() {
        use crate::xml::XmlDocumentParser;
        let mut source=Document::new(128);let mut target=Document::new(128);
        let (mut parser,script)=XmlDocumentParser::start(&mut source,
            "<h:html xmlns:h='http://www.w3.org/1999/xhtml' xmlns:p='urn:original'><h:body><h:section id='moved'><h:script/>text<![CDATA[cdata]]><?instruction data?><!--comment--><p:item p:attribute='value'/></h:section><h:p id='source'>tail</h:p></h:body></h:html>").unwrap();
        assert!(script.is_some());
        let section=crate::selector::query_selector(&source,source.root(),"#moved").unwrap().unwrap();
        let (moved,mapping)=target.adopt_subtree_from(&mut source,section).unwrap();
        target.append(target.root(),moved).unwrap();
        target.set_attribute_ns(moved,Some("http://www.w3.org/2000/xmlns/"),"xmlns:p","urn:mutated").unwrap();
        parser.project_retained_nodes(|node|mapping.iter().find_map(|(old,new)|(*old==node).then_some(*new)).unwrap_or(node));
        assert!(parser.has_open_element(moved));
        let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
        assert!(parser.resume(&mut view).unwrap().is_none());drop(view);
        assert!(!parser.has_open_element(moved));
        let mut cursor=target.first_child(moved).unwrap();let mut kinds=alloc::vec::Vec::new();
        while let Some(node)=cursor {assert_eq!(target.node_document(node).unwrap(),target.root());kinds.push(node);cursor=target.next_sibling(node).unwrap();}
        assert_eq!(kinds.len(),6);
        assert!(matches!(target.kind(kinds[1]),Ok(NodeKind::Text(text)) if text=="text"));
        assert!(matches!(target.kind(kinds[2]),Ok(NodeKind::CData(text)) if text=="cdata"));
        assert!(matches!(target.kind(kinds[3]),Ok(NodeKind::ProcessingInstruction{target,data}) if target=="instruction"&&data=="data"));
        assert!(matches!(target.kind(kinds[4]),Ok(NodeKind::Comment(text)) if text=="comment"));
        assert_eq!(target.element_name_parts(kinds[5]).unwrap(),(Some("p"),"item"));
        assert!(matches!(target.kind(kinds[5]),Ok(NodeKind::Element {namespace:Namespace::Other(uri),..}) if uri.as_ref()=="urn:original"));
        assert_eq!(target.get_attribute_ns_ref(kinds[5],Some("urn:original"),"attribute").unwrap(),Some("value"));
        assert!(crate::selector::query_selector(&source,source.root(),"#source").unwrap().is_some());
    }

    #[test]
    fn specification_parser_owner_view_xml_fatal_error_retires_actual_style_owner() {
        use crate::xml::XmlDocumentParser;
        use alloc::rc::Rc;
        use core::cell::Cell;
        let mut source=Document::new(64);let mut target=Document::new(64);
        let retired=Rc::new(Cell::new(None));let observer=retired.clone();
        target.set_parser_style_block_sink(Some(Rc::new(move|document,node| {observer.set(Some((document.root(),node)));Ok(())})));
        let (mut parser,script)=XmlDocumentParser::start(&mut source,
            "<html xmlns='http://www.w3.org/1999/xhtml'><section id='moved'><script/><style>p{color:red}</wrong></section></html>").unwrap();
        assert!(script.is_some());let section=crate::selector::query_selector(&source,source.root(),"#moved").unwrap().unwrap();
        let (moved,mapping)=target.adopt_subtree_from(&mut source,section).unwrap();target.append(target.root(),moved).unwrap();
        parser.project_retained_nodes(|node|mapping.iter().find_map(|(old,new)|(*old==node).then_some(*new)).unwrap_or(node));
        let mut view=ParserDocuments::new(alloc::vec![&mut source,&mut target]).unwrap();
        assert!(parser.resume(&mut view).is_err());assert!(parser.resume(&mut view).unwrap().is_none());drop(view);
        let style=target.last_child(moved).unwrap().unwrap();
        assert_eq!(retired.get(),Some((target.root(),style)));assert!(!parser.has_open_element(style));assert!(!parser.has_open_element(moved));
        let text=target.first_child(style).unwrap().unwrap();assert!(matches!(target.kind(text),Ok(NodeKind::Text(value)) if value=="p{color:red}"));
        assert_eq!(target.next_sibling(style).unwrap(),None);
    }

}
